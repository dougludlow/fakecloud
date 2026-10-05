//! The AWS-managed prefix lists: lists AWS owns (owner `AWS`) for its own
//! services' address ranges, which any account can reference from security
//! group rules and route tables.
//!
//! Each one has a published weight: the number of entries a reference to it
//! takes up in the referencing resource. A security group rule that references
//! the CloudFront origin-facing list counts as 55 rules against "Inbound or
//! outbound rules per security group", not one.
//! Source: <https://docs.aws.amazon.com/vpc/latest/userguide/working-with-aws-managed-prefix-lists.html>.

/// Where a list's address ranges come from in AWS's published IP address
/// ranges (`ip-ranges.json`, vendored as `aws_prefix_lists/ranges.json` by
/// `scripts/gen-aws-prefix-lists.py`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ranges {
    /// Every range of the service, whatever region it is tagged with (the
    /// global CloudFront list).
    All(&'static str),
    /// The service's ranges tagged with the list's region.
    Region(&'static str),
    /// The service's ranges across the list's partition, plus `GLOBAL` in the
    /// `aws` partition: Route 53 health checkers probe from every region, so
    /// each region's list allows all of them.
    Partition(&'static str),
    /// AWS publishes no address ranges for the list.
    Unpublished,
}

/// One AWS-managed prefix list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AwsManagedPrefixList {
    /// The list name; `{region}` stands for the region it lives in.
    name: &'static str,
    pub(crate) address_family: &'static str,
    /// Entries a reference to the list takes up in a resource.
    pub(crate) weight: usize,
    ranges: Ranges,
}

const fn list(
    name: &'static str,
    address_family: &'static str,
    weight: usize,
    ranges: Ranges,
) -> AwsManagedPrefixList {
    AwsManagedPrefixList {
        name,
        address_family,
        weight,
        ranges,
    }
}

/// Every AWS-managed prefix list with its published weight.
pub(crate) const AWS_MANAGED_PREFIX_LISTS: &[AwsManagedPrefixList] = &[
    list(
        "com.amazonaws.global.cloudfront.origin-facing",
        "IPv4",
        55,
        Ranges::All("CLOUDFRONT_ORIGIN_FACING"),
    ),
    list(
        "com.amazonaws.global.ipv6.cloudfront.origin-facing",
        "IPv6",
        55,
        Ranges::All("CLOUDFRONT_ORIGIN_FACING"),
    ),
    list(
        "com.amazonaws.{region}.dynamodb",
        "IPv4",
        1,
        Ranges::Region("DYNAMODB"),
    ),
    list(
        "com.amazonaws.{region}.ec2-instance-connect",
        "IPv4",
        2,
        Ranges::Region("EC2_INSTANCE_CONNECT"),
    ),
    list(
        "com.amazonaws.{region}.ipv6.ec2-instance-connect",
        "IPv6",
        2,
        Ranges::Region("EC2_INSTANCE_CONNECT"),
    ),
    list(
        "com.amazonaws.global.groundstation",
        "IPv4",
        5,
        Ranges::Unpublished,
    ),
    list(
        "com.amazonaws.{region}.route53-healthchecks",
        "IPv4",
        25,
        Ranges::Partition("ROUTE53_HEALTHCHECKS"),
    ),
    list(
        "com.amazonaws.{region}.ipv6.route53-healthchecks",
        "IPv6",
        25,
        Ranges::Partition("ROUTE53_HEALTHCHECKS"),
    ),
    list("com.amazonaws.{region}.s3", "IPv4", 1, Ranges::Region("S3")),
    list(
        "com.amazonaws.{region}.s3express",
        "IPv4",
        6,
        Ranges::Unpublished,
    ),
    list(
        "com.amazonaws.{region}.secretsmanager-managed-external-secrets",
        "IPv4",
        20,
        Ranges::Unpublished,
    ),
    list(
        "com.amazonaws.{region}.vpc-lattice",
        "IPv4",
        10,
        Ranges::Unpublished,
    ),
    list(
        "com.amazonaws.{region}.ipv6.vpc-lattice",
        "IPv6",
        10,
        Ranges::Unpublished,
    ),
];

/// `family -> service -> region -> CIDRs`, from the vendored ranges.
type RangeTable = std::collections::BTreeMap<
    String,
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<String>>>,
>;

fn range_table() -> &'static RangeTable {
    static TABLE: std::sync::OnceLock<RangeTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("aws_prefix_lists/ranges.json"))
                .expect("vendored ranges.json is valid JSON");
        let mut table = RangeTable::new();
        for family in ["ipv4", "ipv6"] {
            if let Some(services) = raw.get(family) {
                table.insert(
                    family.to_string(),
                    serde_json::from_value(services.clone())
                        .expect("vendored ranges.json has the generator's shape"),
                );
            }
        }
        table
    })
}

/// The partition a region (or ip-ranges `GLOBAL`) belongs to. The European
/// Sovereign Cloud (`eusc-*`) is its own partition with its own health
/// checkers.
fn partition(region: &str) -> &'static str {
    if region == "GLOBAL" {
        "aws"
    } else if region.starts_with("eusc-") {
        "aws-eusc"
    } else {
        fakecloud_aws::arn::partition_for(region)
    }
}

/// The gateway-endpoint lists the legacy `DescribePrefixLists` reports.
pub(crate) const GATEWAY_ENDPOINT_SERVICES: &[&str] = &["s3", "dynamodb"];

impl AwsManagedPrefixList {
    /// The list's address ranges in `region`, sorted.
    pub(crate) fn cidrs_in(&self, region: &str) -> Vec<String> {
        let family = if self.address_family == "IPv6" {
            "ipv6"
        } else {
            "ipv4"
        };
        let service = match self.ranges {
            Ranges::All(service) | Ranges::Region(service) | Ranges::Partition(service) => service,
            Ranges::Unpublished => return Vec::new(),
        };
        let keep = |r: &str| match self.ranges {
            Ranges::Region(_) => r == region,
            Ranges::Partition(_) => partition(r) == partition(region),
            _ => true,
        };
        let mut out: Vec<String> = range_table()
            .get(family)
            .and_then(|services| services.get(service))
            .into_iter()
            .flat_map(|regions| regions.iter())
            .filter(|(r, _)| keep(r))
            .flat_map(|(_, cidrs)| cidrs.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

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

    fn named(name: &str) -> &'static AwsManagedPrefixList {
        AWS_MANAGED_PREFIX_LISTS
            .iter()
            .find(|l| l.name_in("us-east-1") == name)
            .unwrap()
    }

    #[test]
    fn published_lists_carry_their_published_ranges() {
        let cf = named("com.amazonaws.global.cloudfront.origin-facing").cidrs_in("us-east-1");
        assert!(!cf.is_empty());
        assert!(cf.iter().all(|c| c.contains('.')), "{cf:?}");
        // The global list is the same everywhere.
        assert_eq!(
            cf,
            named("com.amazonaws.global.cloudfront.origin-facing").cidrs_in("eu-west-1")
        );
        let cf6 = named("com.amazonaws.global.ipv6.cloudfront.origin-facing").cidrs_in("us-east-1");
        assert!(!cf6.is_empty());
        assert!(cf6.iter().all(|c| c.contains(':')), "{cf6:?}");

        let s3 = named("com.amazonaws.us-east-1.s3").cidrs_in("us-east-1");
        assert!(s3.contains(&"52.216.0.0/15".to_string()), "{s3:?}");
        assert!(named("com.amazonaws.us-east-1.dynamodb")
            .cidrs_in("us-east-1")
            .contains(&"52.94.0.0/22".to_string()));
        assert!(!named("com.amazonaws.us-east-1.ec2-instance-connect")
            .cidrs_in("us-east-1")
            .is_empty());

        // Route 53 health checkers probe from every region of the partition.
        let r53 = named("com.amazonaws.us-east-1.route53-healthchecks").cidrs_in("eu-central-1");
        assert!(r53.contains(&"15.177.0.0/18".to_string()), "{r53:?}");
        assert!(r53.iter().all(|c| !c.starts_with("52.80.")), "no cn ranges");
        let cn = named("com.amazonaws.us-east-1.route53-healthchecks").cidrs_in("cn-north-1");
        assert!(!cn.is_empty() && !cn.contains(&"15.177.0.0/18".to_string()));
    }

    /// Outside the gateway-endpoint lists (weight 1, as published, however
    /// many ranges they hold), a list never holds more ranges than it weighs.
    #[test]
    fn ranges_fit_the_published_weight() {
        for region in ["us-east-1", "eu-west-1", "ap-southeast-2", "cn-north-1"] {
            for l in AWS_MANAGED_PREFIX_LISTS {
                if matches!(l.ranges, Ranges::Region("S3" | "DYNAMODB")) {
                    continue;
                }
                let n = l.cidrs_in(region).len();
                assert!(n <= l.weight, "{} in {region}: {n} > {}", l.name, l.weight);
            }
        }
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
