//! The Service Quotas quotas EC2 enforces.
//!
//! Applied values come from Service Quotas through
//! [`fakecloud_core::quota::QuotaProvider`], so a quota raised with
//! `RequestServiceQuotaIncrease` changes what EC2 accepts. Without a provider
//! (a bare `Ec2Service`) the published AWS defaults apply.

use std::sync::Arc;

use fakecloud_core::quota::QuotaUsageSource;
use std::collections::HashMap;

use fakecloud_core::service::AwsServiceError;

use crate::service::Ec2Service;
use crate::service_helpers::indexed_list;
use crate::state::{SecurityGroupRule, SharedEc2State};

const VPC: &str = "vpc";
/// `vpc` L-2AFB9258: security groups per network interface.
const SECURITY_GROUPS_PER_INTERFACE: &str = "L-2AFB9258";
/// `vpc` L-0EA8095F: inbound or outbound rules per security group.
const RULES_PER_SECURITY_GROUP: &str = "L-0EA8095F";

/// AWS default for security groups per network interface.
pub(crate) const DEFAULT_SECURITY_GROUPS_PER_INTERFACE: usize = 5;
/// AWS default for inbound (or outbound) rules per security group.
pub(crate) const DEFAULT_RULES_PER_SECURITY_GROUP: usize = 60;

impl Ec2Service {
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
            .map(|v| v.max(0.0) as usize)
            .unwrap_or(default)
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
}

/// Rules one direction of a group counts against the rules-per-group quota.
///
/// AWS enforces the quota separately for IPv4 and IPv6 rules, and a rule that
/// references a security group or prefix list counts toward both, so the
/// binding count is the larger of the two.
pub(crate) fn direction_rule_count<'a>(
    rules: impl Iterator<Item = &'a SecurityGroupRule>,
) -> usize {
    let (mut v4, mut v6) = (0usize, 0usize);
    for r in rules {
        if r.cidr_ipv6.is_none() {
            v4 += 1;
        }
        if r.cidr_ipv4.is_none() {
            v6 += 1;
        }
    }
    v4.max(v6)
}

/// The binding rule count of a group: the busier direction.
pub(crate) fn group_rule_count(rules: &[SecurityGroupRule]) -> usize {
    direction_rule_count(rules.iter().filter(|r| !r.is_egress))
        .max(direction_rule_count(rules.iter().filter(|r| r.is_egress)))
}

fn bad_request(code: &str, message: String) -> AwsServiceError {
    AwsServiceError::aws_error(http::StatusCode::BAD_REQUEST, code, message)
}

/// `SecurityGroupsPerInterfaceLimitExceeded` when a network interface would
/// carry more groups than the applied quota allows.
pub(crate) fn check_groups_per_interface(
    svc: &Ec2Service,
    account_id: &str,
    region: &str,
    requested: usize,
) -> Result<(), AwsServiceError> {
    let limit = svc.security_groups_per_interface(account_id, region);
    if requested > limit {
        return Err(bad_request(
            "SecurityGroupsPerInterfaceLimitExceeded",
            format!(
                "The maximum number of security groups per interface has been reached: \
                 requested {requested}, limit {limit}"
            ),
        ));
    }
    Ok(())
}

/// `SecurityGroupsPerInstanceLimitExceeded` when an instance would carry more
/// groups than its network interface may.
pub(crate) fn check_groups_per_instance(
    svc: &Ec2Service,
    account_id: &str,
    region: &str,
    requested: usize,
) -> Result<(), AwsServiceError> {
    let limit = svc.security_groups_per_interface(account_id, region);
    if requested > limit {
        return Err(bad_request(
            "SecurityGroupsPerInstanceLimitExceeded",
            format!(
                "The maximum number of security groups per instance has been reached: \
                 requested {requested}, limit {limit}"
            ),
        ));
    }
    Ok(())
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
    let instance_level = distinct(indexed_list(params, "SecurityGroupId"))
        + distinct(indexed_list(params, "SecurityGroup"));
    check_groups_per_instance(svc, account_id, region, instance_level)?;
    let mut interfaces: Vec<&str> = params
        .keys()
        .filter_map(|k| k.strip_prefix("NetworkInterface."))
        .filter_map(|rest| rest.split_once('.').map(|(n, _)| n))
        .collect();
    interfaces.sort_unstable();
    interfaces.dedup();
    for n in interfaces {
        let groups = indexed_list(params, &format!("NetworkInterface.{n}.SecurityGroupId"));
        check_groups_per_interface(svc, account_id, region, distinct(groups))?;
    }
    Ok(())
}

fn distinct(mut ids: Vec<String>) -> usize {
    ids.sort_unstable();
    ids.dedup();
    ids.len()
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
    use fakecloud_core::quota::QuotaProvider;

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
        assert_eq!(group_rule_count(&rules), 3);
    }

    struct Fixed(f64);
    impl QuotaProvider for Fixed {
        fn applied_value(&self, _: &str, _: &str, _: &str, _: &str) -> Option<f64> {
            Some(self.0)
        }
    }

    #[test]
    fn provider_overrides_the_defaults() {
        let req = crate::test_support::ec2_request("DescribeVpcs", &[]);
        let svc = Ec2Service::new();
        let (a, r) = (req.account_id.as_str(), req.region.as_str());
        assert_eq!(svc.security_groups_per_interface(a, r), 5);
        assert_eq!(svc.rules_per_security_group(a, r), 60);
        let svc = Ec2Service::new().with_quota_provider(Some(Arc::new(Fixed(8.0))));
        assert_eq!(svc.security_groups_per_interface(a, r), 8);
        assert!(check_groups_per_interface(&svc, a, r, 8).is_ok());
        assert_eq!(
            check_groups_per_interface(&svc, a, r, 9)
                .unwrap_err()
                .code(),
            "SecurityGroupsPerInterfaceLimitExceeded"
        );
    }
}
