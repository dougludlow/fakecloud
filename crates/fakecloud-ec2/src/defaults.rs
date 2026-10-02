//! Default-VPC bootstrap.
//!
//! Every real AWS account has, per region, a *default VPC* (`172.31.0.0/16`)
//! with an attached internet gateway, a main route table that sends
//! `0.0.0.0/0` at the gateway, one default subnet per Availability Zone, a
//! `default` security group, and a default network ACL. Callers that never
//! touch the VPC APIs (the common case — `RunInstances` with no `SubnetId`)
//! still expect their instances to land in that default VPC and come back from
//! `DescribeInstances` with a real `vpc-…` / `subnet-…`.
//!
//! fakecloud builds the same fixtures the first time an account's EC2 state is
//! constructed ([`Ec2State::new`](crate::state::Ec2State::new)). The resource
//! ids are **deterministic** functions of the account id and a role string
//! (region-independent — see [`deterministic_id`]), so the throwaway empty
//! states that the read paths synthesize as a "not found" fallback report the
//! *same* ids as the persisted account state regardless of the caller's region.
//!
//! Per-VPC packet isolation (issue #1745 phase 2+) keys off this topology: a
//! subnet whose route table has a `0.0.0.0/0 -> igw-…` route is public and gets
//! a routable backing network; a subnet without one is private (`internal`).

use crate::state::{
    Ec2State, Image, InternetGateway, NetworkAcl, NetworkAclAssoc, NetworkAclEntry, Route,
    RouteTable, RouteTableAssociation, SecurityGroup, SecurityGroupRule, Subnet, Vpc,
};

/// CIDR of the default VPC, matching AWS.
const DEFAULT_VPC_CIDR: &str = "172.31.0.0/16";

/// The Availability Zone suffixes that receive a default subnet. AWS creates a
/// default subnet in every AZ; three covers the cardinality every realistic
/// test exercises (and keeps the deterministic CIDR layout simple).
const DEFAULT_AZ_SUFFIXES: [&str; 3] = ["a", "b", "c"];

/// Deterministic EC2 resource id: `<prefix>-<17 hex>` derived from the account
/// and a per-resource `role`. Deliberately **region-independent**: a
/// `MultiAccountState` partitions by account and pins a single region per
/// server, but read handlers build a throwaway `Ec2State::new(account,
/// req.region)` for accounts that don't exist yet — where `req.region` is the
/// caller's SigV4 scope, not the server's region. Seeding the id on the region
/// made those throwaway-derived ids disagree with the persisted account's ids
/// whenever the client region differed from the server's, so a no-subnet launch
/// stamped the instance with a subnet/VPC id that didn't exist in its own
/// account (bug-hunt 2026-06-18 finding 1.1). Dropping region from the seed
/// makes both paths agree; the AZ/CIDR cosmetics below still use the region.
pub(crate) fn deterministic_id(prefix: &str, account: &str, role: &str) -> String {
    let seed = format!("{account}/{role}");
    let h1 = fnv1a64(seed.as_bytes());
    let h2 = fnv1a64(format!("{seed}/salt").as_bytes());
    // 16 hex from the first hash + 1 nibble from the second = the 17 hex chars
    // a modern EC2 long-id carries.
    format!("{prefix}-{:016x}{:01x}", h1, h2 & 0xf)
}

/// FNV-1a 64-bit. A tiny, dependency-free, stable hash — we only need
/// determinism, not cryptographic strength.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// AWS's zone-id prefix for a region: the geography code followed by the
/// abbreviated direction and the number (`us-east-1 -> use1`,
/// `ap-southeast-2 -> apse2`, `ap-south-1 -> aps1`, `us-gov-west-1 ->
/// usgw1`). Compound directions keep both letters, which is what keeps
/// ap-south-1 (`aps1`) and ap-southeast-1 (`apse1`) apart.
pub(crate) fn az_id_prefix(region: &str) -> String {
    let parts: Vec<&str> = region.split('-').collect();
    if parts.len() < 3 || parts.iter().any(|p| p.is_empty()) {
        return region.replace('-', "");
    }
    let mut out = String::from(parts[0]);
    for part in &parts[1..parts.len() - 1] {
        let abbrev = match *part {
            "east" => "e",
            "west" => "w",
            "north" => "n",
            "south" => "s",
            "central" => "c",
            "northeast" => "ne",
            "northwest" => "nw",
            "southeast" => "se",
            "southwest" => "sw",
            "gov" => "g",
            other => {
                // First character, not first byte: the input can be any
                // caller-supplied zone name.
                out.extend(other.chars().next());
                continue;
            }
        };
        out.push_str(abbrev);
    }
    out.push_str(parts[parts.len() - 1]);
    out
}

/// Number of `-`-separated parts that make up the Region in a zone name:
/// `us-east-1` (3) or `us-gov-west-1` (4).
fn region_part_count(parts: &[&str]) -> usize {
    if parts.get(1) == Some(&"gov") {
        4
    } else {
        3
    }
}

/// The zone id of an availability zone name, using the same letter-to-number
/// mapping DescribeAvailabilityZones reports: `us-east-1b -> use1-az2`, and a
/// Local Zone `us-west-2-lax-1a -> usw2-lax1-az1`.
pub(crate) fn zone_id_for(zone: &str) -> String {
    let parts: Vec<&str> = zone.split('-').collect();
    let n = region_part_count(&parts);
    if parts.len() < n {
        return format!("{}-az1", az_id_prefix(zone));
    }
    // The zone letter closes the last part: `1b`, or `1a` of `lax-1a`.
    let mut tail = parts[n - 1..].concat();
    let letter = match tail.pop() {
        Some(c) if c.is_ascii_lowercase() => c,
        Some(c) => {
            tail.push(c);
            'a'
        }
        None => 'a',
    };
    let az = u32::from(letter) - u32::from('a') + 1;
    let region_number = &parts[n - 1];
    let region = format!(
        "{}-{}",
        parts[..n - 1].join("-"),
        region_number.trim_end_matches(|c: char| c.is_ascii_lowercase())
    );
    let local = &tail[region_number
        .trim_end_matches(|c: char| c.is_ascii_lowercase())
        .len()..];
    if local.is_empty() {
        format!("{}-az{az}", az_id_prefix(&region))
    } else {
        format!("{}-{local}-az{az}", az_id_prefix(&region))
    }
}

/// Whether `zone` is an availability zone (or Local Zone) of `region`:
/// `{region}{letter}`, or `{region}-{location}-{n}{letter}`.
pub(crate) fn zone_in_region(region: &str, zone: &str) -> bool {
    let Some(rest) = zone.strip_prefix(region) else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => chars.next().is_none(),
        Some('-') => {
            let local = &rest[1..];
            local.ends_with(|c: char| c.is_ascii_lowercase())
                && local.split('-').count() == 2
                && local
                    .split('-')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
        }
        _ => false,
    }
}

/// The availability-zone name a zone id denotes in `region`
/// (`use1-az2 -> us-east-1b`), or `None` when the id belongs to another
/// region or is malformed.
pub(crate) fn zone_name_for_id(region: &str, zone_id: &str) -> Option<String> {
    let (prefix, n) = zone_id.rsplit_once("-az")?;
    if prefix != az_id_prefix(region) {
        return None;
    }
    let n: u32 = n.parse().ok()?;
    if !(1..=26).contains(&n) {
        return None;
    }
    let letter = char::from_u32(u32::from('a') + n - 1)?;
    Some(format!("{region}{letter}"))
}

/// The default VPC id for an account (also exposed so request handlers can
/// resolve the implicit default without re-deriving the seed by hand).
pub(crate) fn default_vpc_id(account: &str) -> String {
    deterministic_id("vpc", account, "default-vpc")
}

/// The default security-group id for an account.
pub(crate) fn default_security_group_id(account: &str) -> String {
    deterministic_id("sg", account, "default-sg")
}

/// Populate `state` with the default VPC topology. Called once at state
/// construction; idempotent in practice because the deterministic ids collide
/// on re-entry.
/// Seed a small catalogue of public AMIs the way every real AWS account sees
/// them, so Terraform's `aws_ami` / `aws_ami_ids` data sources — which resolve
/// an image via `owners = ["amazon"|"099720109477"|…]` + a `name` wildcard +
/// `most_recent = true` — return a result instead of empty. Without this,
/// `DescribeImages` only ever returned user-registered AMIs, so the common
/// `data "aws_ami" "al2" { most_recent = true; owners = ["amazon"]; filter { … } }`
/// pattern (and everything that chains off it, e.g. an `aws_instance` or an
/// ELBv2 target-group attachment) could not be planned.
///
/// Ids and `creationDate`s are deterministic and version-stable; the distinct
/// dates make `most_recent` ordering well-defined. These are public, read-only
/// fixtures owned by Amazon/Canonical — not user data — and share the
/// deterministic-id property of the default network so the throwaway empty
/// states the read paths build report the same catalogue.
/// One seeded public-AMI row: `(image_id, name, description, architecture,
/// owner_id, owner_alias, creation_date, root_device_name, platform)`.
type AmiSeed = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
    &'static str,
    &'static str,
    Option<&'static str>,
);

pub(crate) fn seed_public_images(state: &mut Ec2State) {
    const AMAZON: &str = "137112412989";
    const CANONICAL: &str = "099720109477";
    const AMAZON_WINDOWS: &str = "801119661308";
    let seeds: &[AmiSeed] = &[
        // Amazon Linux 2 (x86_64 + arm64). The `amzn2-ami-minimal-hvm-*` name +
        // `root-device-type = ebs` shape is exactly what the standard
        // terraform-provider-aws acctest helper
        // `ConfigLatestAmazonLinux2HVMEBSX8664AMI()` / `…ARM64AMI()` filters on,
        // so every acceptance test that launches an instance via that helper
        // (aws_instance, the ELBv2 target-group attachment, autoscaling, …)
        // resolves a real AMI from this catalogue.
        (
            "ami-0a1b2c3d4e5f60001",
            "amzn2-ami-minimal-hvm-2.0.20240306.2-x86_64-ebs",
            "Amazon Linux 2 AMI 2.0.20240306.2 x86_64 Minimal HVM ebs",
            "x86_64",
            AMAZON,
            Some("amazon"),
            "2024-03-06T12:00:00.000Z",
            "/dev/xvda",
            None,
        ),
        (
            "ami-0a1b2c3d4e5f60007",
            "amzn2-ami-minimal-hvm-2.0.20240306.2-arm64-ebs",
            "Amazon Linux 2 AMI 2.0.20240306.2 arm64 Minimal HVM ebs",
            "arm64",
            AMAZON,
            Some("amazon"),
            "2024-03-06T12:00:00.000Z",
            "/dev/xvda",
            None,
        ),
        (
            "ami-0a1b2c3d4e5f60002",
            "al2023-ami-2023.4.20240319.1-kernel-6.1-x86_64",
            "Amazon Linux 2023 AMI 2023.4.20240319.1 x86_64 HVM kernel-6.1",
            "x86_64",
            AMAZON,
            Some("amazon"),
            "2024-03-19T12:00:00.000Z",
            "/dev/xvda",
            None,
        ),
        (
            "ami-0a1b2c3d4e5f60003",
            "al2023-ami-2023.4.20240319.1-kernel-6.1-arm64",
            "Amazon Linux 2023 AMI 2023.4.20240319.1 arm64 HVM kernel-6.1",
            "arm64",
            AMAZON,
            Some("amazon"),
            "2024-03-19T12:00:00.000Z",
            "/dev/xvda",
            None,
        ),
        (
            "ami-0a1b2c3d4e5f60004",
            "ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-20240319",
            "Canonical, Ubuntu, 22.04 LTS, amd64 jammy image build on 2024-03-19",
            "x86_64",
            CANONICAL,
            None,
            "2024-03-19T06:00:00.000Z",
            "/dev/sda1",
            None,
        ),
        (
            "ami-0a1b2c3d4e5f60005",
            "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-20240423",
            "Canonical, Ubuntu, 24.04 LTS, amd64 noble image build on 2024-04-23",
            "x86_64",
            CANONICAL,
            None,
            "2024-04-23T06:00:00.000Z",
            "/dev/sda1",
            None,
        ),
        // Canonical instance-store Ubuntu (the `ubuntu/images/hvm-instance/*`
        // shape the `aws_ami_ids` acctest filters on, distinct from the EBS
        // `hvm-ssd*` images above).
        (
            "ami-0a1b2c3d4e5f60008",
            "ubuntu/images/hvm-instance/ubuntu-jammy-22.04-amd64-server-20240319",
            "Canonical, Ubuntu, 22.04 LTS, amd64 jammy instance-store image build on 2024-03-19",
            "x86_64",
            CANONICAL,
            None,
            "2024-03-19T05:00:00.000Z",
            "/dev/sda1",
            None,
        ),
        (
            "ami-0a1b2c3d4e5f60006",
            "Windows_Server-2022-English-Full-Base-2024.03.13",
            "Microsoft Windows Server 2022 Full Locale English AMI provided by Amazon",
            "x86_64",
            AMAZON_WINDOWS,
            Some("amazon"),
            "2024-03-13T12:00:00.000Z",
            "/dev/sda1",
            Some("windows"),
        ),
    ];
    for (id, name, desc, arch, owner, alias, created, root_dev, platform) in seeds {
        state.images.insert(
            (*id).to_string(),
            Image {
                image_id: (*id).to_string(),
                name: (*name).to_string(),
                description: (*desc).to_string(),
                state: "available".to_string(),
                architecture: (*arch).to_string(),
                public: true,
                source_instance_id: None,
                in_recycle_bin: false,
                deprecation_time: None,
                deregistration_protection: false,
                launch_permission_users: Vec::new(),
                launch_permission_groups: vec!["all".to_string()],
                boot_mode: None,
                owner_id: Some((*owner).to_string()),
                supported_instance_types: Vec::new(),
                unsupported_instance_types: Vec::new(),
                owner_alias: alias.map(str::to_string),
                creation_date: Some((*created).to_string()),
                root_device_name: Some((*root_dev).to_string()),
                platform: platform.map(str::to_string),
            },
        );
    }
}

pub(crate) fn bootstrap_default_network(state: &mut Ec2State) {
    let account = state.account_id.clone();
    let region = if state.region.is_empty() {
        "us-east-1".to_string()
    } else {
        state.region.clone()
    };

    let vpc_id = default_vpc_id(&account);
    let igw_id = deterministic_id("igw", &account, "default-igw");
    let rtb_id = deterministic_id("rtb", &account, "default-rtb");
    let acl_id = deterministic_id("acl", &account, "default-acl");
    let sg_id = default_security_group_id(&account);

    // --- default VPC ---
    state.vpcs.insert(
        vpc_id.clone(),
        Vpc {
            vpc_id: vpc_id.clone(),
            cidr_block: DEFAULT_VPC_CIDR.to_string(),
            state: "available".to_string(),
            dhcp_options_id: "default".to_string(),
            instance_tenancy: "default".to_string(),
            is_default: true,
            enable_dns_support: true,
            enable_dns_hostnames: true,
            enable_network_address_usage_metrics: false,
            cidr_associations: Vec::new(),
            ipv6_cidr_block: None,
        },
    );

    // --- internet gateway, attached to the default VPC ---
    state.internet_gateways.insert(
        igw_id.clone(),
        InternetGateway {
            internet_gateway_id: igw_id.clone(),
            attachments: vec![(vpc_id.clone(), "available".to_string())],
        },
    );

    // --- default subnets, one per AZ ---
    let az_prefix = az_id_prefix(&region);
    let mut subnet_ids = Vec::new();
    for (idx, suffix) in DEFAULT_AZ_SUFFIXES.iter().enumerate() {
        let subnet_id = deterministic_id("subnet", &account, &format!("default-subnet-{suffix}"));
        let az = format!("{region}{suffix}");
        state.subnets.insert(
            subnet_id.clone(),
            Subnet {
                subnet_id: subnet_id.clone(),
                vpc_id: vpc_id.clone(),
                // /20 blocks carved from 172.31.0.0/16: .0, .16, .32 …
                cidr_block: format!("172.31.{}.0/20", idx * 16),
                availability_zone: az,
                availability_zone_id: format!("{az_prefix}-az{}", idx + 1),
                state: "available".to_string(),
                available_ip_address_count: 4091,
                default_for_az: true,
                // Default subnets auto-assign public IPs, matching AWS.
                map_public_ip_on_launch: true,
                assign_ipv6_address_on_creation: false,
                map_customer_owned_ip_on_launch: false,
                enable_dns64: false,
                enable_resource_name_dns_a_record_on_launch: false,
                enable_resource_name_dns_aaaa_record_on_launch: false,
                private_dns_hostname_type: "ip-name".to_string(),
                ipv6_cidr_block: None,
            },
        );
        subnet_ids.push(subnet_id);
    }

    // --- main route table: local + default route at the IGW (public) ---
    let mut associations = vec![RouteTableAssociation {
        association_id: deterministic_id("rtbassoc", &account, "default-rtb-main"),
        route_table_id: rtb_id.clone(),
        subnet_id: None,
        gateway_id: None,
        main: true,
    }];
    for sid in &subnet_ids {
        associations.push(RouteTableAssociation {
            association_id: deterministic_id("rtbassoc", &account, &format!("default-rtb-{sid}")),
            route_table_id: rtb_id.clone(),
            subnet_id: Some(sid.clone()),
            gateway_id: None,
            main: false,
        });
    }
    state.route_tables.insert(
        rtb_id.clone(),
        RouteTable {
            route_table_id: rtb_id.clone(),
            vpc_id: vpc_id.clone(),
            routes: vec![
                Route {
                    destination_cidr_block: Some(DEFAULT_VPC_CIDR.to_string()),
                    gateway_id: Some("local".to_string()),
                    ..Default::default()
                },
                Route {
                    destination_cidr_block: Some("0.0.0.0/0".to_string()),
                    gateway_id: Some(igw_id.clone()),
                    ..Default::default()
                },
            ],
            associations,
        },
    );

    // --- default security group: allow all from self, allow all egress ---
    state.security_groups.insert(
        sg_id.clone(),
        SecurityGroup {
            group_id: sg_id.clone(),
            group_name: "default".to_string(),
            description: "default VPC security group".to_string(),
            vpc_id: vpc_id.clone(),
            rules: vec![
                SecurityGroupRule {
                    rule_id: deterministic_id("sgr", &account, "default-sg-ingress"),
                    group_id: sg_id.clone(),
                    is_egress: false,
                    ip_protocol: "-1".to_string(),
                    from_port: -1,
                    to_port: -1,
                    cidr_ipv4: None,
                    cidr_ipv6: None,
                    prefix_list_id: None,
                    referenced_group_id: Some(sg_id.clone()),
                    referenced_group_name: None,
                    referenced_user_id: None,
                    description: String::new(),
                },
                SecurityGroupRule {
                    rule_id: deterministic_id("sgr", &account, "default-sg-egress"),
                    group_id: sg_id.clone(),
                    is_egress: true,
                    ip_protocol: "-1".to_string(),
                    from_port: -1,
                    to_port: -1,
                    cidr_ipv4: Some("0.0.0.0/0".to_string()),
                    cidr_ipv6: None,
                    prefix_list_id: None,
                    referenced_group_id: None,
                    referenced_group_name: None,
                    referenced_user_id: None,
                    description: String::new(),
                },
            ],
        },
    );

    // --- default network ACL: allow-all, associated with every default subnet ---
    let nacl_associations = subnet_ids
        .iter()
        .map(|sid| NetworkAclAssoc {
            association_id: deterministic_id("aclassoc", &account, &format!("default-acl-{sid}")),
            subnet_id: sid.clone(),
        })
        .collect();
    state.network_acls.insert(
        acl_id.clone(),
        NetworkAcl {
            network_acl_id: acl_id.clone(),
            vpc_id: vpc_id.clone(),
            is_default: true,
            entries: vec![
                allow_all_entry(false),
                deny_all_entry(false),
                allow_all_entry(true),
                deny_all_entry(true),
            ],
            associations: nacl_associations,
        },
    );
}

/// Create the implicit resources AWS provisions for every newly-created VPC: a
/// `default` security group, a default network ACL, and a main route table
/// (with the `local` route). The `aws_vpc` resource reads back
/// `default_security_group_id`, `default_network_acl_id`,
/// `default_route_table_id`, and `main_route_table_id`, all derived from these.
/// Ids are deterministic functions of the VPC id so read-path fallbacks agree.
pub(crate) fn create_vpc_default_resources(state: &mut Ec2State, vpc_id: &str, cidr: &str) {
    let account = state.account_id.clone();
    let rtb_id = deterministic_id("rtb", &account, &format!("{vpc_id}-main-rtb"));
    let acl_id = deterministic_id("acl", &account, &format!("{vpc_id}-default-acl"));
    let sg_id = deterministic_id("sg", &account, &format!("{vpc_id}-default-sg"));

    state
        .route_tables
        .entry(rtb_id.clone())
        .or_insert_with(|| RouteTable {
            route_table_id: rtb_id.clone(),
            vpc_id: vpc_id.to_string(),
            routes: vec![Route {
                destination_cidr_block: Some(cidr.to_string()),
                gateway_id: Some("local".to_string()),
                ..Default::default()
            }],
            associations: vec![RouteTableAssociation {
                association_id: deterministic_id("rtbassoc", &account, &format!("{vpc_id}-main")),
                route_table_id: rtb_id.clone(),
                subnet_id: None,
                gateway_id: None,
                main: true,
            }],
        });

    state
        .security_groups
        .entry(sg_id.clone())
        .or_insert_with(|| SecurityGroup {
            group_id: sg_id.clone(),
            group_name: "default".to_string(),
            description: "default VPC security group".to_string(),
            vpc_id: vpc_id.to_string(),
            rules: vec![
                SecurityGroupRule {
                    rule_id: deterministic_id("sgr", &account, &format!("{vpc_id}-sg-ingress")),
                    group_id: sg_id.clone(),
                    is_egress: false,
                    ip_protocol: "-1".to_string(),
                    from_port: -1,
                    to_port: -1,
                    cidr_ipv4: None,
                    cidr_ipv6: None,
                    prefix_list_id: None,
                    referenced_group_id: Some(sg_id.clone()),
                    referenced_group_name: None,
                    referenced_user_id: None,
                    description: String::new(),
                },
                SecurityGroupRule {
                    rule_id: deterministic_id("sgr", &account, &format!("{vpc_id}-sg-egress")),
                    group_id: sg_id.clone(),
                    is_egress: true,
                    ip_protocol: "-1".to_string(),
                    from_port: -1,
                    to_port: -1,
                    cidr_ipv4: Some("0.0.0.0/0".to_string()),
                    cidr_ipv6: None,
                    prefix_list_id: None,
                    referenced_group_id: None,
                    referenced_group_name: None,
                    referenced_user_id: None,
                    description: String::new(),
                },
            ],
        });

    state
        .network_acls
        .entry(acl_id.clone())
        .or_insert_with(|| NetworkAcl {
            network_acl_id: acl_id.clone(),
            vpc_id: vpc_id.to_string(),
            is_default: true,
            entries: vec![
                allow_all_entry(false),
                deny_all_entry(false),
                allow_all_entry(true),
                deny_all_entry(true),
            ],
            associations: Vec::new(),
        });
}

fn allow_all_entry(egress: bool) -> NetworkAclEntry {
    NetworkAclEntry {
        rule_number: 100,
        protocol: "-1".to_string(),
        rule_action: "allow".to_string(),
        egress,
        cidr_block: Some("0.0.0.0/0".to_string()),
        ipv6_cidr_block: None,
        port_range: None,
        icmp_type_code: None,
    }
}

fn deny_all_entry(egress: bool) -> NetworkAclEntry {
    NetworkAclEntry {
        rule_number: 32767,
        protocol: "-1".to_string(),
        rule_action: "deny".to_string(),
        egress,
        cidr_block: Some("0.0.0.0/0".to_string()),
        ipv6_cidr_block: None,
        port_range: None,
        icmp_type_code: None,
    }
}

/// True when `subnet_id` resolves to a subnet whose route table carries a
/// `0.0.0.0/0` route at an internet gateway — i.e. a *public* subnet. A subnet
/// without such a route is private and (phase 2) backs onto an `internal`
/// network. Subnets default to their VPC's main route table when not
/// explicitly associated.
// Drives per-subnet networking (chooses `internal` vs routable backing
// networks) from `service/mod.rs` and `instance.rs`.
pub(crate) fn subnet_is_public(state: &Ec2State, subnet_id: &str) -> bool {
    let Some(subnet) = state.subnets.get(subnet_id) else {
        return false;
    };
    // An explicit association wins; otherwise fall back to the VPC's main table.
    let explicit = state.route_tables.values().find(|rt| {
        rt.associations
            .iter()
            .any(|a| a.subnet_id.as_deref() == Some(subnet_id))
    });
    let main = state
        .route_tables
        .values()
        .find(|rt| rt.vpc_id == subnet.vpc_id && rt.associations.iter().any(|a| a.main));
    let rt = explicit.or(main);
    rt.map(route_table_has_igw_default).unwrap_or(false)
}

fn route_table_has_igw_default(rt: &RouteTable) -> bool {
    rt.routes.iter().any(|r| {
        r.destination_cidr_block.as_deref() == Some("0.0.0.0/0")
            && r.gateway_id
                .as_deref()
                .map(|g| g.starts_with("igw-"))
                .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Ec2State;

    #[test]
    fn deterministic_id_is_stable_and_shaped() {
        let a = deterministic_id("vpc", "123456789012", "default-vpc");
        let b = deterministic_id("vpc", "123456789012", "default-vpc");
        assert_eq!(a, b);
        assert!(a.starts_with("vpc-"));
        // 17 hex chars after the prefix, matching EC2 long-ids.
        let hex = a.strip_prefix("vpc-").unwrap();
        assert_eq!(hex.len(), 17);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn deterministic_id_varies_by_account_and_role() {
        let base = deterministic_id("vpc", "111111111111", "default-vpc");
        assert_ne!(base, deterministic_id("vpc", "222222222222", "default-vpc"));
        assert_ne!(base, deterministic_id("vpc", "111111111111", "default-igw"));
    }

    #[test]
    fn deterministic_id_is_region_independent() {
        // The id seed deliberately excludes region so read-path (req.region)
        // and persisted (server region) states agree (finding 1.1). Region is
        // not a parameter anymore — this documents the contract by asserting
        // default_vpc_id depends only on the account.
        assert_eq!(
            default_vpc_id("111111111111"),
            deterministic_id("vpc", "111111111111", "default-vpc")
        );
    }

    #[test]
    fn az_id_prefix_matches_aws_shape() {
        for (region, prefix) in [
            ("us-east-1", "use1"),
            ("us-east-2", "use2"),
            ("us-west-1", "usw1"),
            ("us-west-2", "usw2"),
            ("eu-west-2", "euw2"),
            ("eu-central-1", "euc1"),
            ("eu-north-1", "eun1"),
            ("eu-south-2", "eus2"),
            ("ap-south-1", "aps1"),
            ("ap-south-2", "aps2"),
            ("ap-southeast-1", "apse1"),
            ("ap-southeast-2", "apse2"),
            ("ap-southeast-7", "apse7"),
            ("ap-northeast-1", "apne1"),
            ("ap-northeast-3", "apne3"),
            ("ap-east-1", "ape1"),
            ("af-south-1", "afs1"),
            ("ca-central-1", "cac1"),
            ("ca-west-1", "caw1"),
            ("sa-east-1", "sae1"),
            ("me-south-1", "mes1"),
            ("me-central-1", "mec1"),
            ("il-central-1", "ilc1"),
            ("mx-central-1", "mxc1"),
            ("cn-north-1", "cnn1"),
            ("cn-northwest-1", "cnnw1"),
            ("us-gov-west-1", "usgw1"),
            ("us-gov-east-1", "usge1"),
        ] {
            assert_eq!(az_id_prefix(region), prefix, "{region}");
        }
    }

    #[test]
    fn zone_ids_round_trip_through_zone_names() {
        assert_eq!(zone_id_for("us-east-1a"), "use1-az1");
        assert_eq!(zone_id_for("us-west-2-lax-1a"), "usw2-lax1-az1");
        assert_eq!(zone_id_for("us-gov-west-1b"), "usgw1-az2");
        assert_eq!(zone_id_for("ap-southeast-2c"), "apse2-az3");
        assert_eq!(zone_id_for("ap-south-1b"), "aps1-az2");
        assert_eq!(
            zone_name_for_id("ap-southeast-2", "apse2-az3").as_deref(),
            Some("ap-southeast-2c")
        );
        // An id from another region is not a zone of this one.
        assert_eq!(zone_name_for_id("ap-southeast-1", "aps1-az1"), None);
        assert_eq!(zone_name_for_id("us-east-1", "use1-az0"), None);
        assert_eq!(zone_name_for_id("us-east-1", "garbage"), None);
    }

    #[test]
    fn non_ascii_zone_names_do_not_panic() {
        // Caller-supplied names reach these helpers; a multi-byte character
        // must not be sliced mid-codepoint.
        assert_eq!(az_id_prefix("us-\u{e9}-1"), "us\u{e9}1");
        let _ = zone_id_for("us-\u{e9}-1a");
        let _ = zone_id_for("\u{e9}");
        assert!(!zone_in_region("us-east-1", "us-east-1\u{e9}"));
    }

    #[test]
    fn zone_membership_follows_the_region() {
        assert!(zone_in_region("us-east-1", "us-east-1a"));
        assert!(zone_in_region("us-east-1", "us-east-1f"));
        assert!(zone_in_region("us-west-2", "us-west-2-lax-1a"));
        assert!(!zone_in_region("us-east-1", "us-west-2a"));
        assert!(!zone_in_region("us-east-1", "us-east-1"));
        assert!(!zone_in_region("us-east-1", "us-east-1ab"));
        assert!(!zone_in_region("us-east-1", "us-east-12a"));
        assert!(!zone_in_region("us-east-1", "string"));
    }

    #[test]
    fn bootstrap_creates_full_default_topology() {
        let state = Ec2State::new("123456789012", "us-east-1");
        // exactly one VPC, marked default
        assert_eq!(state.vpcs.len(), 1);
        let vpc = state.vpcs.values().next().unwrap();
        assert!(vpc.is_default);
        assert_eq!(vpc.cidr_block, "172.31.0.0/16");
        // one subnet per AZ suffix, all default_for_az + public
        assert_eq!(state.subnets.len(), DEFAULT_AZ_SUFFIXES.len());
        assert!(state.subnets.values().all(|s| s.default_for_az));
        assert!(state.subnets.values().all(|s| s.map_public_ip_on_launch));
        // IGW attached to the default VPC
        assert_eq!(state.internet_gateways.len(), 1);
        let igw = state.internet_gateways.values().next().unwrap();
        assert_eq!(igw.attachments[0].0, vpc.vpc_id);
        // default SG + default NACL
        let sg = state.security_groups.values().next().unwrap();
        assert_eq!(sg.group_name, "default");
        assert!(state.network_acls.values().next().unwrap().is_default);
    }

    #[test]
    fn default_subnets_are_public() {
        let state = Ec2State::new("123456789012", "us-east-1");
        for sid in state.subnets.keys() {
            assert!(
                subnet_is_public(&state, sid),
                "subnet {sid} should be public"
            );
        }
    }

    #[test]
    fn ids_match_across_fresh_states() {
        // The throwaway "empty" states read paths build must agree with the
        // persisted account state on the default VPC id.
        let a = Ec2State::new("123456789012", "us-east-1");
        let b = Ec2State::new("123456789012", "us-east-1");
        let a_vpc: Vec<_> = a.vpcs.keys().collect();
        let b_vpc: Vec<_> = b.vpcs.keys().collect();
        assert_eq!(a_vpc, b_vpc);
        assert_eq!(a_vpc[0], &default_vpc_id("123456789012"));
    }

    #[test]
    fn default_vpc_id_agrees_across_regions() {
        // The crux of finding 1.1: a read-path empty built with the caller's
        // region must derive the SAME default VPC id as the persisted account
        // state built with the server's region.
        let read_path = Ec2State::new("123456789012", "eu-west-1");
        let persisted = Ec2State::new("123456789012", "us-east-1");
        let read_vpc = read_path.vpcs.keys().next().unwrap();
        let persisted_vpc = persisted.vpcs.keys().next().unwrap();
        assert_eq!(read_vpc, persisted_vpc);
        // subnets too (so a no-subnet launch resolves a subnet that exists in
        // the persisted account regardless of the caller's region).
        let read_subnets: std::collections::BTreeSet<_> = read_path.subnets.keys().collect();
        let persisted_subnets: std::collections::BTreeSet<_> = persisted.subnets.keys().collect();
        assert_eq!(read_subnets, persisted_subnets);
    }
}
