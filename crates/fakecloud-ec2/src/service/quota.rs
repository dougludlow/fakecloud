//! The Service Quotas quotas EC2 enforces.
//!
//! Applied values come from Service Quotas through
//! [`fakecloud_core::quota::QuotaProvider`], so a quota raised with
//! `RequestServiceQuotaIncrease` changes what EC2 accepts. Enforcement is
//! opt-in: a quota is only checked once the user switched it on, and a bare
//! `Ec2Service` (no provider) enforces nothing. The published AWS defaults
//! still answer `ValidateSecurityGroupQuotasForInterface` without a provider.

use std::sync::Arc;

use fakecloud_core::quota::QuotaUsageSource;
use std::collections::{BTreeMap, HashMap};

use fakecloud_core::service::AwsServiceError;

use crate::service::Ec2Service;
use crate::service_helpers::indexed_list;
use crate::state::{ManagedPrefixList, SecurityGroupRule, SharedEc2State};

pub(crate) use fakecloud_core::quota::{
    DEFAULT_RULES_PER_SECURITY_GROUP, DEFAULT_SECURITY_GROUPS_PER_INTERFACE,
};
use fakecloud_core::quota::{
    RULES_PER_SECURITY_GROUP, SECURITY_GROUPS_PER_INTERFACE, VPC_SERVICE_CODE as VPC,
};

fn as_count(v: f64) -> usize {
    v.max(0.0) as usize
}

impl Ec2Service {
    /// The applied value of a VPC quota, whether or not it is enforced.
    fn applied_quota(
        &self,
        account_id: &str,
        region: &str,
        quota_code: &str,
        default: usize,
    ) -> usize {
        self.quota_provider
            .as_ref()
            .and_then(|p| p.applied_value(account_id, region, VPC, quota_code))
            .map(as_count)
            .unwrap_or(default)
    }

    /// The limit of a quota EC2 enforces for this account, `None` while the
    /// quota is not enforced.
    pub(crate) fn enforced_quota(
        &self,
        account_id: &str,
        region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<usize> {
        self.quota_provider
            .as_ref()
            .and_then(|p| p.enforced_limit(account_id, region, service_code, quota_code))
            .map(as_count)
    }

    /// Applied value of "Security groups per network interface".
    pub(crate) fn security_groups_per_interface(&self, account_id: &str, region: &str) -> usize {
        self.applied_quota(
            account_id,
            region,
            SECURITY_GROUPS_PER_INTERFACE,
            DEFAULT_SECURITY_GROUPS_PER_INTERFACE,
        )
    }

    /// Applied value of "Inbound or outbound rules per security group".
    pub(crate) fn rules_per_security_group(&self, account_id: &str, region: &str) -> usize {
        self.applied_quota(
            account_id,
            region,
            RULES_PER_SECURITY_GROUP,
            DEFAULT_RULES_PER_SECURITY_GROUP,
        )
    }

    /// "Security groups per network interface" when it is enforced.
    pub(crate) fn enforced_security_groups_per_interface(
        &self,
        account_id: &str,
        region: &str,
    ) -> Option<usize> {
        self.enforced_quota(account_id, region, VPC, SECURITY_GROUPS_PER_INTERFACE)
    }

    /// "Inbound or outbound rules per security group" when it is enforced.
    pub(crate) fn enforced_rules_per_security_group(
        &self,
        account_id: &str,
        region: &str,
    ) -> Option<usize> {
        self.enforced_quota(account_id, region, VPC, RULES_PER_SECURITY_GROUP)
    }
}

/// How much each rule weighs against the rules-per-group quota.
///
/// A CIDR or security-group rule weighs one. A rule that references a
/// customer-managed prefix list weighs the list's `MaxEntries`, as on AWS, and
/// counts only toward the list's address family. A prefix list fakecloud does
/// not hold (an AWS-managed one) weighs one on both sides.
#[derive(Clone, Copy)]
pub(crate) struct RuleWeights<'a> {
    prefix_lists: &'a BTreeMap<String, ManagedPrefixList>,
}

impl<'a> RuleWeights<'a> {
    pub(crate) fn new(prefix_lists: &'a BTreeMap<String, ManagedPrefixList>) -> Self {
        Self { prefix_lists }
    }

    /// The rule's IPv4-side and IPv6-side weight.
    fn sides(&self, r: &SecurityGroupRule) -> [usize; 2] {
        if let Some(list) = r
            .prefix_list_id
            .as_ref()
            .and_then(|id| self.prefix_lists.get(id))
        {
            let weight = list.max_entries.max(1) as usize;
            return if list.address_family.eq_ignore_ascii_case("IPv6") {
                [0, weight]
            } else {
                [weight, 0]
            };
        }
        [
            usize::from(r.cidr_ipv6.is_none()),
            usize::from(r.cidr_ipv4.is_none()),
        ]
    }

    /// IPv4-side and IPv6-side rule counts of one direction's rules.
    pub(crate) fn side_counts<'r>(
        &self,
        rules: impl Iterator<Item = &'r SecurityGroupRule>,
    ) -> [usize; 2] {
        rules.fold([0, 0], |[v4, v6], r| {
            let [w4, w6] = self.sides(r);
            [v4 + w4, v6 + w6]
        })
    }

    /// Rules one direction of a group counts against the quota. AWS enforces
    /// it separately for IPv4 and IPv6 rules, and a security-group reference
    /// counts toward both, so the binding count is the larger of the two.
    pub(crate) fn direction_rule_count<'r>(
        &self,
        rules: impl Iterator<Item = &'r SecurityGroupRule>,
    ) -> usize {
        let [v4, v6] = self.side_counts(rules);
        v4.max(v6)
    }

    /// The binding rule count of a group: the busier direction.
    pub(crate) fn group_rule_count(&self, rules: &[SecurityGroupRule]) -> usize {
        self.direction_rule_count(rules.iter().filter(|r| !r.is_egress))
            .max(self.direction_rule_count(rules.iter().filter(|r| r.is_egress)))
    }

    /// The first quota side (direction x IP version) an edit grows past
    /// `limit`, as `(count, direction)`. A side that was already over the
    /// limit and does not grow is left alone: the edit did not cause it.
    pub(crate) fn side_grown_past(
        &self,
        before: &[SecurityGroupRule],
        after: &[SecurityGroupRule],
        limit: usize,
    ) -> Option<(usize, &'static str)> {
        for (egress, direction) in [(false, "inbound"), (true, "outbound")] {
            let b = self.side_counts(before.iter().filter(|r| r.is_egress == egress));
            let a = self.side_counts(after.iter().filter(|r| r.is_egress == egress));
            for side in 0..2 {
                if a[side] > limit && a[side] > b[side] {
                    return Some((a[side], direction));
                }
            }
        }
        None
    }
}

fn bad_request(code: &str, message: String) -> AwsServiceError {
    AwsServiceError::aws_error(http::StatusCode::BAD_REQUEST, code, message)
}

/// Which kind of resource a security-group count is for: AWS reports the
/// same quota under a different error code for each.
#[derive(Clone, Copy)]
pub(crate) enum GroupHolder {
    Interface,
    Instance,
}

/// `SecurityGroupsPerInterfaceLimitExceeded` (or `...PerInstance...` for an
/// instance) when more groups are requested than `limit` allows. A `None`
/// limit (the quota is not enforced) accepts any count.
pub(crate) fn check_group_count(
    limit: Option<usize>,
    holder: GroupHolder,
    requested: usize,
) -> Result<(), AwsServiceError> {
    let Some(limit) = limit.filter(|l| requested > *l) else {
        return Ok(());
    };
    let (code, noun) = match holder {
        GroupHolder::Interface => ("SecurityGroupsPerInterfaceLimitExceeded", "interface"),
        GroupHolder::Instance => ("SecurityGroupsPerInstanceLimitExceeded", "instance"),
    };
    Err(bad_request(
        code,
        format!(
            "The maximum number of security groups per {noun} has been reached: requested \
             {requested}, limit {limit}"
        ),
    ))
}

/// Groups a request names, counted once each: AWS does not count a repeated
/// id twice.
pub(crate) fn distinct_count(ids: &[String]) -> usize {
    let mut ids: Vec<&String> = ids.iter().collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

/// Security-group counts of a launch (`RunInstances` parameters, after any
/// launch template is merged in): the instance-level groups, and each network
/// interface's.
pub(crate) fn check_launch_groups(
    svc: &Ec2Service,
    account_id: &str,
    region: &str,
    params: &HashMap<String, String>,
) -> Result<(), AwsServiceError> {
    let limit = svc.enforced_security_groups_per_interface(account_id, region);
    if limit.is_none() {
        return Ok(());
    }
    let mut ids = indexed_list(params, "SecurityGroupId");
    let names = indexed_list(params, "SecurityGroup");
    if !names.is_empty() {
        // A group named by `SecurityGroup.N` is the same group when its id is
        // also listed, so resolve names to ids before counting.
        let accounts = svc.state.read();
        if let Some(state) = accounts.get(account_id) {
            for name in &names {
                let resolved: Vec<&String> = state
                    .security_groups
                    .values()
                    .filter(|g| &g.group_name == name)
                    .map(|g| &g.group_id)
                    .collect();
                if !resolved.iter().any(|id| ids.contains(id)) {
                    ids.push(format!("name:{name}"));
                }
            }
        } else {
            ids.extend(names.iter().map(|n| format!("name:{n}")));
        }
    }
    check_group_count(limit, GroupHolder::Instance, distinct_count(&ids))?;
    let mut interfaces: Vec<&str> = params
        .keys()
        .filter_map(|k| k.strip_prefix("NetworkInterface."))
        .filter_map(|rest| rest.split_once('.').map(|(n, _)| n))
        .collect();
    interfaces.sort_unstable();
    interfaces.dedup();
    for n in interfaces {
        let groups = indexed_list(params, &format!("NetworkInterface.{n}.SecurityGroupId"));
        check_group_count(limit, GroupHolder::Interface, distinct_count(&groups))?;
    }
    Ok(())
}

/// `RulesPerSecurityGroupLimitExceeded`.
pub(crate) fn rules_limit_exceeded(message: String) -> AwsServiceError {
    bad_request("RulesPerSecurityGroupLimitExceeded", message)
}

/// Counts the EC2 resources behind region-level quotas, so Service Quotas
/// utilization reports show real usage.
pub struct Ec2QuotaUsage {
    state: SharedEc2State,
}

impl Ec2QuotaUsage {
    pub fn new(state: SharedEc2State) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

impl QuotaUsageSource for Ec2QuotaUsage {
    fn service_codes(&self) -> &[&str] {
        &["ec2", VPC]
    }

    fn usage(
        &self,
        account_id: &str,
        _region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64> {
        let accounts = self.state.read();
        let state = accounts.get(account_id);
        let count = |n: Option<usize>| Some(n.unwrap_or(0) as f64);
        match (service_code, quota_code) {
            (VPC, "L-F678F1CE") => count(state.map(|s| s.vpcs.len())),
            (VPC, "L-A4707A72") => count(state.map(|s| s.internet_gateways.len())),
            (VPC, "L-E79EC296") => count(state.map(|s| s.security_groups.len())),
            (VPC, "L-DF5E4CA3") => count(state.map(|s| s.network_interfaces.len())),
            (VPC, "L-45FE3B85") => count(state.map(|s| s.egress_only_igws.len())),
            ("ec2", "L-0263D0A3") => count(state.map(|s| s.elastic_ips.len())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        is_egress: bool,
        v4: Option<&str>,
        v6: Option<&str>,
        sg: Option<&str>,
    ) -> SecurityGroupRule {
        SecurityGroupRule {
            rule_id: "sgr-1".into(),
            group_id: "sg-1".into(),
            is_egress,
            ip_protocol: "tcp".into(),
            from_port: 1,
            to_port: 1,
            cidr_ipv4: v4.map(Into::into),
            cidr_ipv6: v6.map(Into::into),
            prefix_list_id: None,
            referenced_group_id: sg.map(Into::into),
            referenced_group_name: None,
            referenced_user_id: None,
            description: String::new(),
        }
    }

    #[test]
    fn ipv4_and_ipv6_are_counted_separately_and_references_count_for_both() {
        let rules = vec![
            rule(false, Some("10.0.0.0/8"), None, None),
            rule(false, Some("10.1.0.0/16"), None, None),
            rule(false, None, Some("::/0"), None),
            rule(false, None, None, Some("sg-2")),
            rule(true, None, Some("::/0"), None),
        ];
        // Ingress: 3 IPv4-side (two CIDRs + the reference), 2 IPv6-side.
        assert_eq!(
            RuleWeights::new(&BTreeMap::new()).group_rule_count(&rules),
            3
        );
    }

    #[test]
    fn an_edit_is_judged_on_the_side_it_grows() {
        // Ingress already holds 3 IPv4 rules (over a limit of 2); egress has
        // 2 IPv4 rules. Moving one egress rule to IPv6 grows nothing past 2.
        let mut before = vec![
            rule(false, Some("10.0.0.0/8"), None, None),
            rule(false, Some("10.1.0.0/16"), None, None),
            rule(false, Some("10.2.0.0/16"), None, None),
            rule(true, Some("10.0.0.0/8"), None, None),
            rule(true, Some("10.1.0.0/16"), None, None),
        ];
        let mut after = before.clone();
        after[4].cidr_ipv4 = None;
        after[4].cidr_ipv6 = Some("::/0".into());
        let w = BTreeMap::new();
        let w = RuleWeights::new(&w);
        assert_eq!(w.side_grown_past(&before, &after, 2), None);
        // Egress growing to 3 IPv6 rules is caught even though ingress, the
        // busier direction, does not change.
        before.push(rule(true, None, Some("::/1"), None));
        before.push(rule(true, None, Some("::/2"), None));
        let mut after = before.clone();
        after[3].cidr_ipv4 = None;
        after[3].cidr_ipv6 = Some("::/3".into());
        assert_eq!(w.side_grown_past(&before, &after, 2), Some((3, "outbound")));
    }

    #[test]
    fn a_prefix_list_rule_weighs_its_max_entries_on_its_family() {
        let mut lists = BTreeMap::new();
        lists.insert(
            "pl-1".to_string(),
            ManagedPrefixList {
                prefix_list_id: "pl-1".into(),
                prefix_list_name: "corp".into(),
                address_family: "IPv4".into(),
                max_entries: 10,
                version: 1,
                state: "create-complete".into(),
                state_message: None,
                entries: Vec::new(),
                version_history: BTreeMap::new(),
            },
        );
        let mut pl = rule(false, None, None, None);
        pl.prefix_list_id = Some("pl-1".into());
        let mut unknown = rule(false, None, None, None);
        unknown.prefix_list_id = Some("pl-aws".into());
        let rules = [pl, unknown, rule(false, None, Some("::/0"), None)];
        let w = RuleWeights::new(&lists);
        // IPv4: 10 (pl-1) + 1 (unknown list); IPv6: 1 (unknown list) + 1.
        assert_eq!(w.side_counts(rules.iter()), [11, 2]);
    }

    #[test]
    fn nothing_is_enforced_without_a_provider() {
        let req = crate::test_support::ec2_request("DescribeVpcs", &[]);
        let svc = Ec2Service::new();
        let (a, r) = (req.account_id.as_str(), req.region.as_str());
        assert_eq!(svc.enforced_security_groups_per_interface(a, r), None);
        assert_eq!(svc.enforced_rules_per_security_group(a, r), None);
        // The applied values still answer validation.
        assert_eq!(svc.security_groups_per_interface(a, r), 5);
        assert_eq!(svc.rules_per_security_group(a, r), 60);
        let limit = svc.enforced_security_groups_per_interface(a, r);
        assert!(check_group_count(limit, GroupHolder::Interface, 100).is_ok());
    }

    #[test]
    fn provider_limits_are_enforced() {
        let req = crate::test_support::ec2_request("DescribeVpcs", &[]);
        let svc = Ec2Service::new().with_quota_provider(Some(Arc::new(
            fakecloud_core::quota::FixedQuotas::default().with(
                VPC,
                SECURITY_GROUPS_PER_INTERFACE,
                8.0,
            ),
        )));
        let (a, r) = (req.account_id.as_str(), req.region.as_str());
        assert_eq!(svc.security_groups_per_interface(a, r), 8);
        let limit = svc.enforced_security_groups_per_interface(a, r);
        assert_eq!(limit, Some(8));
        // A quota the provider does not enforce stays unchecked.
        assert_eq!(svc.enforced_rules_per_security_group(a, r), None);
        assert!(check_group_count(limit, GroupHolder::Interface, 8).is_ok());
        assert_eq!(
            check_group_count(limit, GroupHolder::Interface, 9)
                .unwrap_err()
                .code(),
            "SecurityGroupsPerInterfaceLimitExceeded"
        );
    }
}
