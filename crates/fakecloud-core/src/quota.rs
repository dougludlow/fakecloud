//! Cross-service Service Quotas wiring.
//!
//! Service Quotas owns the applied value of every quota (the AWS default,
//! raised by an approved `RequestServiceQuotaIncrease` or an Organizations
//! quota request template, or set directly through the introspection API) and
//! whether fakecloud enforces it. Enforcement is opt-in: a quota is enforced
//! only when the user turned it on, for every quota (`--enforce-quotas`) or
//! for that quota alone. Services that enforce a quota ask for its limit
//! through [`QuotaProvider::enforced_limit`], so raising a quota through the
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

    /// The applied value of `service_code`/`quota_code` when fakecloud
    /// enforces that quota for `account_id`, or `None` when it does not (the
    /// quota is not switched on, or unknown). A service refuses a request
    /// that would take it past the returned limit.
    fn enforced_limit(
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

/// A [`QuotaProvider`] with fixed values that enforces every quota it holds,
/// for wiring a service without Service Quotas (unit tests, embedders).
#[derive(Debug, Clone, Default)]
pub struct FixedQuotas {
    quotas: Vec<(String, String, f64)>,
}

impl FixedQuotas {
    /// Enforce `service_code`/`quota_code` at `value`.
    pub fn with(mut self, service_code: &str, quota_code: &str, value: f64) -> Self {
        self.quotas
            .retain(|(s, q, _)| !(s == service_code && q == quota_code));
        self.quotas
            .push((service_code.to_string(), quota_code.to_string(), value));
        self
    }

    /// The two security-group quotas, enforced at their AWS defaults.
    pub fn security_group_defaults() -> Self {
        Self::default()
            .with(
                VPC_SERVICE_CODE,
                SECURITY_GROUPS_PER_INTERFACE,
                DEFAULT_SECURITY_GROUPS_PER_INTERFACE as f64,
            )
            .with(
                VPC_SERVICE_CODE,
                RULES_PER_SECURITY_GROUP,
                DEFAULT_RULES_PER_SECURITY_GROUP as f64,
            )
    }
}

impl QuotaProvider for FixedQuotas {
    fn applied_value(&self, _: &str, _: &str, service_code: &str, quota_code: &str) -> Option<f64> {
        self.quotas
            .iter()
            .find(|(s, q, _)| s == service_code && q == quota_code)
            .map(|(_, _, v)| *v)
    }

    fn enforced_limit(
        &self,
        account_id: &str,
        region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64> {
        self.applied_value(account_id, region, service_code, quota_code)
    }
}
