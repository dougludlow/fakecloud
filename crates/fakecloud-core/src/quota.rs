//! Cross-service Service Quotas wiring.
//!
//! Service Quotas owns the applied value of every quota (the AWS default,
//! raised by an approved `RequestServiceQuotaIncrease` or an Organizations
//! quota request template). Services that enforce a quota ask it for the
//! current value through [`QuotaProvider`], so raising a quota through the
//! Service Quotas API changes what the enforcing service accepts.
//!
//! The reverse direction is [`QuotaUsageSource`]: a service that can count its
//! own resources reports the usage behind a quota, which Service Quotas turns
//! into a quota utilization report.

/// Service code of the Amazon VPC quotas.
pub const VPC_SERVICE_CODE: &str = "vpc";
/// `vpc` quota: security groups per network interface.
pub const SECURITY_GROUPS_PER_INTERFACE: &str = "L-2AFB9258";
/// `vpc` quota: inbound or outbound rules per security group.
pub const RULES_PER_SECURITY_GROUP: &str = "L-0EA8095F";
/// AWS default for [`SECURITY_GROUPS_PER_INTERFACE`].
pub const DEFAULT_SECURITY_GROUPS_PER_INTERFACE: usize = 5;
/// AWS default for [`RULES_PER_SECURITY_GROUP`].
pub const DEFAULT_RULES_PER_SECURITY_GROUP: usize = 60;

/// Service code of the IAM quotas.
pub const IAM_SERVICE_CODE: &str = "iam";

/// An IAM `GetAccountSummary` `*Quota` entry backed by a Service Quotas quota.
#[derive(Debug, Clone, Copy)]
pub struct IamSummaryQuota {
    /// The `SummaryMap` key, e.g. `RolesQuota`.
    pub summary_key: &'static str,
    /// The `iam` Service Quotas quota code.
    pub quota_code: &'static str,
    /// The AWS default for a new account.
    pub default: f64,
}

/// The `GetAccountSummary` quota entries IAM reads from Service Quotas, so a
/// quota raised with `RequestServiceQuotaIncrease` shows up in IAM. The other
/// `*Quota` entries (policy sizes, access keys per user, ...) are fixed AWS
/// limits with no Service Quotas counterpart.
pub const IAM_SUMMARY_QUOTAS: &[IamSummaryQuota] = &[
    IamSummaryQuota {
        summary_key: "UsersQuota",
        quota_code: "L-F55AF5E4",
        default: 5000.0,
    },
    IamSummaryQuota {
        summary_key: "GroupsQuota",
        quota_code: "L-F4A5425F",
        default: 300.0,
    },
    IamSummaryQuota {
        summary_key: "ServerCertificatesQuota",
        quota_code: "L-BF35879D",
        default: 20.0,
    },
    IamSummaryQuota {
        summary_key: "PoliciesQuota",
        quota_code: "L-E95E4862",
        default: 1500.0,
    },
    IamSummaryQuota {
        summary_key: "RolesQuota",
        quota_code: "L-FE177D64",
        default: 1000.0,
    },
    IamSummaryQuota {
        summary_key: "InstanceProfilesQuota",
        quota_code: "L-6E65F664",
        default: 1000.0,
    },
    IamSummaryQuota {
        summary_key: "AttachedPoliciesPerGroupQuota",
        quota_code: "L-384571C4",
        default: 10.0,
    },
    IamSummaryQuota {
        summary_key: "AttachedPoliciesPerRoleQuota",
        quota_code: "L-0DA4ABF3",
        default: 20.0,
    },
    IamSummaryQuota {
        summary_key: "AttachedPoliciesPerUserQuota",
        quota_code: "L-4019AD8B",
        default: 10.0,
    },
    IamSummaryQuota {
        summary_key: "AssumeRolePolicySizeQuota",
        quota_code: "L-C07B4B0D",
        default: 2048.0,
    },
];

/// Resolves the applied value of a quota for an account.
pub trait QuotaProvider: Send + Sync {
    /// The applied value of `service_code`/`quota_code` for `account_id` in
    /// `region`, or `None` when the quota is unknown to Service Quotas.
    fn applied_value(
        &self,
        account_id: &str,
        region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64>;
}

/// Reports how much of a quota an account currently uses.
pub trait QuotaUsageSource: Send + Sync {
    /// The service code (`vpc`, `ec2`, ...) whose quotas this source counts.
    fn service_codes(&self) -> &[&str];

    /// Current usage of `service_code`/`quota_code` for `account_id` in
    /// `region`, or `None` when this source cannot measure that quota.
    fn usage(
        &self,
        account_id: &str,
        region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64>;
}
