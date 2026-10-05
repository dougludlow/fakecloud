//! The quotas Service Quotas knows about: their codes, names, AWS default
//! values and whether they can be raised.
//!
//! Values are the published AWS defaults for a new account. Quotas another
//! fakecloud service can enforce are marked [`QuotaDef::enforceable`]; the
//! enforcing service reads their applied value from here through
//! [`fakecloud_core::quota::QuotaProvider`] once the user switched
//! enforcement on.

/// A service that has quotas.
#[derive(Debug, Clone, Copy)]
pub struct ServiceDef {
    pub code: &'static str,
    pub name: &'static str,
}

/// The CloudWatch metric that reports a quota's usage.
#[derive(Debug, Clone, Copy)]
pub struct UsageMetric {
    pub namespace: &'static str,
    pub name: &'static str,
    pub dimensions: &'static [(&'static str, &'static str)],
    pub statistic: &'static str,
}

/// One quota.
#[derive(Debug, Clone, Copy)]
pub struct QuotaDef {
    pub service_code: &'static str,
    pub quota_code: &'static str,
    pub name: &'static str,
    pub default: f64,
    pub unit: &'static str,
    pub adjustable: bool,
    /// Global quotas (IAM) apply to the whole account rather than a region.
    pub global: bool,
    /// The highest value an increase request can be approved for, when AWS
    /// documents one. A request above it is not approved.
    pub max_value: Option<f64>,
    pub usage_metric: Option<UsageMetric>,
    /// Whether a fakecloud service checks requests against this quota once
    /// enforcement is switched on for it.
    pub enforceable: bool,
}

pub use fakecloud_core::quota::{
    RULES_PER_SECURITY_GROUP, SECURITY_GROUPS_PER_INTERFACE, VPC_SERVICE_CODE as VPC,
};

pub const EC2: &str = "ec2";
/// AWS caps security groups per interface multiplied by rules per security
/// group at this value; an increase request that would exceed it is denied.
pub const SG_RULES_PRODUCT_LIMIT: f64 = 1000.0;

pub const SERVICES: &[ServiceDef] = &[
    ServiceDef {
        code: "dynamodb",
        name: "Amazon DynamoDB",
    },
    ServiceDef {
        code: EC2,
        name: "Amazon Elastic Compute Cloud (Amazon EC2)",
    },
    ServiceDef {
        code: "iam",
        name: "AWS Identity and Access Management (IAM)",
    },
    ServiceDef {
        code: "kms",
        name: "AWS Key Management Service (AWS KMS)",
    },
    ServiceDef {
        code: "lambda",
        name: "AWS Lambda",
    },
    ServiceDef {
        code: "s3",
        name: "Amazon Simple Storage Service (Amazon S3)",
    },
    ServiceDef {
        code: VPC,
        name: "Amazon Virtual Private Cloud (Amazon VPC)",
    },
];

const fn q(
    service_code: &'static str,
    quota_code: &'static str,
    name: &'static str,
    default: f64,
    adjustable: bool,
) -> QuotaDef {
    QuotaDef {
        service_code,
        quota_code,
        name,
        default,
        unit: "None",
        adjustable,
        global: false,
        max_value: None,
        usage_metric: None,
        enforceable: false,
    }
}

const fn enforceable(mut d: QuotaDef) -> QuotaDef {
    d.enforceable = true;
    d
}

const fn global(mut d: QuotaDef) -> QuotaDef {
    d.global = true;
    d
}

const fn max(mut d: QuotaDef, max_value: f64) -> QuotaDef {
    d.max_value = Some(max_value);
    d
}

const fn unit(mut d: QuotaDef, unit: &'static str) -> QuotaDef {
    d.unit = unit;
    d
}

const ON_DEMAND_VCPU: &[(&str, &str)] = &[
    ("Class", "Standard/OnDemand"),
    ("Resource", "vCPU"),
    ("Service", "EC2"),
    ("Type", "Resource"),
];

const SPOT_VCPU: &[(&str, &str)] = &[
    ("Class", "Standard/Spot"),
    ("Resource", "vCPU"),
    ("Service", "EC2"),
    ("Type", "Resource"),
];

const fn vcpu_metric(
    mut d: QuotaDef,
    dimensions: &'static [(&'static str, &'static str)],
) -> QuotaDef {
    d.usage_metric = Some(UsageMetric {
        namespace: "AWS/Usage",
        name: "ResourceCount",
        dimensions,
        statistic: "Maximum",
    });
    d
}

pub const QUOTAS: &[QuotaDef] = &[
    // ---- Amazon VPC ----
    enforceable(max(
        q(
            VPC,
            SECURITY_GROUPS_PER_INTERFACE,
            "Security groups per network interface",
            fakecloud_core::quota::DEFAULT_SECURITY_GROUPS_PER_INTERFACE as f64,
            true,
        ),
        16.0,
    )),
    enforceable(q(
        VPC,
        RULES_PER_SECURITY_GROUP,
        "Inbound or outbound rules per security group",
        fakecloud_core::quota::DEFAULT_RULES_PER_SECURITY_GROUP as f64,
        true,
    )),
    q(VPC, "L-F678F1CE", "VPCs per Region", 5.0, true),
    q(VPC, "L-A4707A72", "Internet gateways per Region", 5.0, true),
    q(VPC, "L-407747CB", "Subnets per VPC", 200.0, true),
    q(
        VPC,
        "L-E79EC296",
        "VPC security groups per Region",
        2500.0,
        true,
    ),
    q(VPC, "L-589F43AA", "Route tables per VPC", 200.0, true),
    max(
        q(VPC, "L-93826ACB", "Routes per route table", 500.0, true),
        1000.0,
    ),
    q(VPC, "L-B4A6D682", "Network ACLs per VPC", 200.0, true),
    max(
        q(VPC, "L-2AEEBF1A", "Rules per network ACL", 20.0, true),
        40.0,
    ),
    q(
        VPC,
        "L-FE5A380F",
        "NAT gateways per Availability Zone",
        5.0,
        true,
    ),
    q(
        VPC,
        "L-DF5E4CA3",
        "Network interfaces per Region",
        5000.0,
        true,
    ),
    q(
        VPC,
        "L-45FE3B85",
        "Egress-only internet gateways per Region",
        5.0,
        true,
    ),
    max(
        q(VPC, "L-83CA0A9D", "IPv4 CIDR blocks per VPC", 5.0, true),
        50.0,
    ),
    max(
        q(VPC, "L-085A6257", "IPv6 CIDR blocks per VPC", 5.0, true),
        50.0,
    ),
    max(
        q(
            VPC,
            "L-7E9ECCDB",
            "Active VPC peering connections per VPC",
            50.0,
            true,
        ),
        125.0,
    ),
    q(
        VPC,
        "L-DC9F7029",
        "Outstanding VPC peering connection requests",
        25.0,
        true,
    ),
    q(
        VPC,
        "L-8312C5BB",
        "VPC peering connection request expiry hours",
        168.0,
        false,
    ),
    q(
        VPC,
        "L-1B52E74A",
        "Gateway VPC endpoints per Region",
        20.0,
        true,
    ),
    q(
        VPC,
        "L-29B6F2EB",
        "Interface VPC endpoints per VPC",
        50.0,
        true,
    ),
    q(
        VPC,
        "L-3248932A",
        "Characters per VPC endpoint policy",
        20480.0,
        false,
    ),
    max(
        q(VPC, "L-BB24F6E5", "Network Address Usage", 64000.0, true),
        256000.0,
    ),
    max(
        q(
            VPC,
            "L-CD17FD4B",
            "Peered Network Address Usage",
            128000.0,
            true,
        ),
        512000.0,
    ),
    q(
        VPC,
        "L-2C462E13",
        "Participant accounts per VPC",
        100.0,
        true,
    ),
    q(
        VPC,
        "L-44499CD2",
        "Subnets that can be shared with an account",
        100.0,
        true,
    ),
    // ---- Amazon EC2 ----
    vcpu_metric(
        q(
            EC2,
            "L-1216C47A",
            "Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances",
            5.0,
            true,
        ),
        ON_DEMAND_VCPU,
    ),
    vcpu_metric(
        q(
            EC2,
            "L-34B43A08",
            "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests",
            5.0,
            true,
        ),
        SPOT_VCPU,
    ),
    q(EC2, "L-0263D0A3", "EC2-VPC Elastic IPs", 5.0, true),
    q(
        EC2,
        "L-74FC7D96",
        "Running On-Demand F instances",
        0.0,
        true,
    ),
    q(
        EC2,
        "L-DB2E81BA",
        "Running On-Demand G and VT instances",
        0.0,
        true,
    ),
    q(
        EC2,
        "L-1945791B",
        "Running On-Demand Inf instances",
        0.0,
        true,
    ),
    q(
        EC2,
        "L-417A185B",
        "Running On-Demand P instances",
        0.0,
        true,
    ),
    q(
        EC2,
        "L-7295265B",
        "Running On-Demand X instances",
        0.0,
        true,
    ),
    q(
        EC2,
        "L-43DA4232",
        "Running On-Demand High Memory instances",
        0.0,
        true,
    ),
    q(EC2, "L-A2478D36", "Transit gateways per account", 5.0, true),
    q(
        EC2,
        "L-3E6EC3A3",
        "Site-to-Site VPN connections per Region",
        50.0,
        true,
    ),
    // ---- IAM (global) ----
    global(q("iam", "L-F55AF5E4", "Users per account", 5000.0, false)),
    global(max(
        q("iam", "L-FE177D64", "Roles per account", 1000.0, true),
        10000.0,
    )),
    global(max(
        q("iam", "L-F4A5425F", "Groups per account", 300.0, true),
        500.0,
    )),
    global(max(
        q("iam", "L-0DA4ABF3", "Managed policies per role", 20.0, true),
        25.0,
    )),
    global(max(
        q("iam", "L-4019AD8B", "Managed policies per user", 10.0, true),
        20.0,
    )),
    global(max(
        q(
            "iam",
            "L-384571C4",
            "Managed policies per group",
            10.0,
            false,
        ),
        10.0,
    )),
    global(max(
        q(
            "iam",
            "L-E95E4862",
            "Customer managed policies per account",
            1500.0,
            true,
        ),
        10000.0,
    )),
    global(max(
        q(
            "iam",
            "L-BF35879D",
            "Server certificates per account",
            20.0,
            true,
        ),
        20.0,
    )),
    global(max(
        q(
            "iam",
            "L-858F3967",
            "OpenId connect providers per account",
            100.0,
            true,
        ),
        700.0,
    )),
    global(max(
        q(
            "iam",
            "L-6E65F664",
            "Instance profiles per account",
            1000.0,
            true,
        ),
        10000.0,
    )),
    global(max(
        q(
            "iam",
            "L-C07B4B0D",
            "Role trust policy length",
            2048.0,
            true,
        ),
        8192.0,
    )),
    // ---- Lambda ----
    unit(
        q(
            "lambda",
            "L-B99A9384",
            "Concurrent executions",
            1000.0,
            true,
        ),
        "Count",
    ),
    unit(
        q(
            "lambda",
            "L-2ACBD22F",
            "Function and layer storage",
            300.0,
            true,
        ),
        "Gigabytes",
    ),
    // ---- S3 ----
    q("s3", "L-DC2B2D3D", "General purpose buckets", 10000.0, true),
    // ---- DynamoDB ----
    q(
        "dynamodb",
        "L-F98FE922",
        "Maximum number of tables",
        2500.0,
        true,
    ),
    // ---- KMS ----
    q(
        "kms",
        "L-C2F1777E",
        "Customer Master Keys (CMKs)",
        100000.0,
        true,
    ),
];

/// Parse a `service_code/quota_code` reference (as the CLI flags and the
/// introspection API take it) into its catalog entry.
pub fn parse_ref(reference: &str) -> Option<&'static QuotaDef> {
    let (service_code, quota_code) = reference.split_once('/')?;
    quota(service_code, quota_code)
}

pub fn service(code: &str) -> Option<&'static ServiceDef> {
    SERVICES.iter().find(|s| s.code == code)
}

pub fn quota(service_code: &str, quota_code: &str) -> Option<&'static QuotaDef> {
    QUOTAS
        .iter()
        .find(|q| q.service_code == service_code && q.quota_code == quota_code)
}

/// Every quota of `service_code`, ordered by quota code.
pub fn quotas_of(service_code: &str) -> Vec<&'static QuotaDef> {
    let mut out: Vec<&QuotaDef> = QUOTAS
        .iter()
        .filter(|q| q.service_code == service_code)
        .collect();
    out.sort_by(|a, b| a.quota_code.cmp(b.quota_code));
    out
}

pub fn service_name(code: &str) -> &'static str {
    service(code).map(|s| s.name).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_quota_belongs_to_a_listed_service_and_is_unique() {
        for (i, q) in QUOTAS.iter().enumerate() {
            assert!(service(q.service_code).is_some(), "{}", q.quota_code);
            assert!(
                !QUOTAS[..i]
                    .iter()
                    .any(|o| o.service_code == q.service_code && o.quota_code == q.quota_code),
                "duplicate {}",
                q.quota_code
            );
            if let Some(m) = q.max_value {
                assert!(m >= q.default, "{}", q.quota_code);
            }
        }
    }

    /// Defaults, maximums and adjustability as the AWS docs publish them
    /// (Amazon VPC quotas, IAM and STS quotas, Lambda endpoints and quotas).
    #[test]
    fn published_values() {
        let cases: &[(&str, &str, f64, Option<f64>, bool)] = &[
            (VPC, "L-93826ACB", 500.0, Some(1000.0), true),
            (VPC, "L-085A6257", 5.0, Some(50.0), true),
            (VPC, "L-BB24F6E5", 64000.0, Some(256000.0), true),
            (VPC, "L-CD17FD4B", 128000.0, Some(512000.0), true),
            ("iam", "L-0DA4ABF3", 20.0, Some(25.0), true),
            ("iam", "L-4019AD8B", 10.0, Some(20.0), true),
            ("iam", "L-384571C4", 10.0, Some(10.0), false),
            ("iam", "L-FE177D64", 1000.0, Some(10000.0), true),
            ("iam", "L-E95E4862", 1500.0, Some(10000.0), true),
            ("iam", "L-F4A5425F", 300.0, Some(500.0), true),
            ("iam", "L-6E65F664", 1000.0, Some(10000.0), true),
            ("iam", "L-C07B4B0D", 2048.0, Some(8192.0), true),
            ("iam", "L-BF35879D", 20.0, Some(20.0), true),
            ("iam", "L-858F3967", 100.0, Some(700.0), true),
            ("lambda", "L-2ACBD22F", 300.0, None, true),
        ];
        for &(service, code, default, max_value, adjustable) in cases {
            let q = quota(service, code).unwrap_or_else(|| panic!("{code} missing"));
            assert_eq!(q.default, default, "{code} default");
            assert_eq!(q.max_value, max_value, "{code} max");
            assert_eq!(q.adjustable, adjustable, "{code} adjustable");
        }
        assert_eq!(quota("lambda", "L-2ACBD22F").unwrap().unit, "Gigabytes");
        assert_eq!(quota("lambda", "L-B99A9384").unwrap().unit, "Count");
    }

    /// Every IAM `GetAccountSummary` quota IAM resolves through Service
    /// Quotas is a global `iam` quota here, with the same default.
    #[test]
    fn iam_summary_quotas_match_the_catalog() {
        for s in fakecloud_core::quota::IAM_SUMMARY_QUOTAS {
            let q = quota(fakecloud_core::quota::IAM_SERVICE_CODE, s.quota_code)
                .unwrap_or_else(|| panic!("{} missing", s.quota_code));
            assert!(q.global, "{}", s.quota_code);
            assert_eq!(q.default, s.default, "{}", s.summary_key);
        }
    }

    #[test]
    fn default_security_group_quotas_fit_the_product_limit() {
        let groups = quota(VPC, SECURITY_GROUPS_PER_INTERFACE).unwrap().default;
        let rules = quota(VPC, RULES_PER_SECURITY_GROUP).unwrap().default;
        assert_eq!((groups, rules), (5.0, 60.0));
        assert!(groups * rules <= SG_RULES_PRODUCT_LIMIT);
    }
}
