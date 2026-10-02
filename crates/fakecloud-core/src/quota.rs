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
