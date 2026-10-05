//! The AWS-managed prefix lists: lists AWS owns (owner `AWS`) for its own
//! services' address ranges, which any account can reference from security
//! group rules and route tables.
//!
//! Each one has a published weight: the number of entries a reference to it
//! takes up in the referencing resource. A security group rule that references
//! the CloudFront origin-facing list counts as 55 rules against "Inbound or
//! outbound rules per security group", not one.
//! Source: <https://docs.aws.amazon.com/vpc/latest/userguide/working-with-aws-managed-prefix-lists.html>.

/// One AWS-managed prefix list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AwsManagedPrefixList {
    /// The list name; `{region}` stands for the region it lives in.
    name: &'static str,
    pub(crate) address_family: &'static str,
    /// Entries a reference to the list takes up in a resource.
    pub(crate) weight: usize,
    /// Representative CIDRs for the lists `DescribePrefixLists` (the gateway
    /// endpoint lists) reports with their ranges.
    pub(crate) cidrs: &'static [&'static str],
}

const fn list(
    name: &'static str,
    address_family: &'static str,
    weight: usize,
) -> AwsManagedPrefixList {
    AwsManagedPrefixList {
        name,
        address_family,
        weight,
        cidrs: &[],
    }
}

/// Every AWS-managed prefix list with its published weight.
pub(crate) const AWS_MANAGED_PREFIX_LISTS: &[AwsManagedPrefixList] = &[
    list("com.amazonaws.global.cloudfront.origin-facing", "IPv4", 55),
    list(
        "com.amazonaws.global.ipv6.cloudfront.origin-facing",
        "IPv6",
        55,
    ),
    AwsManagedPrefixList {
        name: "com.amazonaws.{region}.dynamodb",
        address_family: "IPv4",
        weight: 1,
        cidrs: &["52.94.0.0/22"],
    },
    list("com.amazonaws.{region}.ec2-instance-connect", "IPv4", 2),
    list(
        "com.amazonaws.{region}.ipv6.ec2-instance-connect",
        "IPv6",
        2,
    ),
    list("com.amazonaws.global.groundstation", "IPv4", 5),
    list("com.amazonaws.{region}.route53-healthchecks", "IPv4", 25),
    list(
        "com.amazonaws.{region}.ipv6.route53-healthchecks",
        "IPv6",
        25,
    ),
    AwsManagedPrefixList {
        name: "com.amazonaws.{region}.s3",
        address_family: "IPv4",
        weight: 1,
        cidrs: &["54.231.0.0/17", "52.216.0.0/15"],
    },
    list("com.amazonaws.{region}.s3express", "IPv4", 6),
    list(
        "com.amazonaws.{region}.secretsmanager-managed-external-secrets",
        "IPv4",
        20,
    ),
    list("com.amazonaws.{region}.vpc-lattice", "IPv4", 10),
    list("com.amazonaws.{region}.ipv6.vpc-lattice", "IPv6", 10),
];

/// The gateway-endpoint lists the legacy `DescribePrefixLists` reports.
pub(crate) const GATEWAY_ENDPOINT_SERVICES: &[&str] = &["s3", "dynamodb"];

impl AwsManagedPrefixList {
    /// The list's name in `region`.
    pub(crate) fn name_in(&self, region: &str) -> String {
        self.name.replace("{region}", region)
    }

    /// The list's id in `region`: deterministic, so every account and every
    /// call sees the same id for the same list, as on AWS.
    pub(crate) fn id_in(&self, region: &str) -> String {
        let key = self
            .name
            .trim_start_matches("com.amazonaws.")
            .replace("{region}.", "");
        let mut hash: u64 = 1469598103934665603;
        for b in format!("{region}.{key}").bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(1099511628257);
        }
        format!("pl-{:08x}", (hash & 0xffff_ffff) as u32)
    }
}

/// The AWS-managed prefix list `id` names in `region`, if any.
pub(crate) fn by_id(region: &str, id: &str) -> Option<&'static AwsManagedPrefixList> {
    if !id.starts_with("pl-") {
        return None;
    }
    AWS_MANAGED_PREFIX_LISTS
        .iter()
        .find(|l| l.id_in(region) == id)
}

/// The gateway-endpoint list for `service` (`s3`, `dynamodb`).
pub(crate) fn gateway_endpoint_list(service: &str) -> Option<&'static AwsManagedPrefixList> {
    let name = format!("com.amazonaws.{{region}}.{service}");
    AWS_MANAGED_PREFIX_LISTS.iter().find(|l| l.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_unique_and_resolve_back() {
        // The gateway-endpoint ids predate this table; they must not move.
        let s3 = gateway_endpoint_list("s3").unwrap();
        assert_eq!(s3.name_in("us-east-1"), "com.amazonaws.us-east-1.s3");
        let mut ids: Vec<String> = AWS_MANAGED_PREFIX_LISTS
            .iter()
            .map(|l| l.id_in("us-east-1"))
            .collect();
        for l in AWS_MANAGED_PREFIX_LISTS {
            let id = l.id_in("us-east-1");
            assert_eq!(by_id("us-east-1", &id).unwrap().name, l.name);
            assert_ne!(id, l.id_in("eu-west-1"), "{}", l.name);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), AWS_MANAGED_PREFIX_LISTS.len());
        assert!(by_id("us-east-1", "pl-unknown").is_none());
    }

    #[test]
    fn cloudfront_origin_facing_weighs_55() {
        let cf = AWS_MANAGED_PREFIX_LISTS
            .iter()
            .find(|l| l.name_in("us-east-1") == "com.amazonaws.global.cloudfront.origin-facing")
            .unwrap();
        assert_eq!((cf.weight, cf.address_family), (55, "IPv4"));
    }
}
