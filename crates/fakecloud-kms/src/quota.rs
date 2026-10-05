//! The Service Quotas quota KMS enforces: "Customer Master Keys (CMKs)"
//! (`kms`/`L-C2F1777E`), the number of customer managed keys per account and
//! Region.
//!
//! As the KMS resource-quota docs put it, the quota applies to every customer
//! managed key in the Region whatever its key spec or key state (a key pending
//! deletion still counts, and so does a multi-Region replica in its own
//! Region); AWS managed keys do not count. Enforcement is opt-in: the quota is
//! only checked once the user switched it on in Service Quotas, and a
//! `KmsService` without a quota provider enforces nothing.

use std::sync::Arc;

use fakecloud_core::quota::{QuotaProvider, QuotaUsageSource};
use fakecloud_core::service::AwsServiceError;
use http::StatusCode;

use crate::state::{KmsState, SharedKmsState};

/// Service code of the KMS quotas.
pub const SERVICE_CODE: &str = "kms";
/// "Customer Master Keys (CMKs)".
pub const CUSTOMER_KEYS: &str = "L-C2F1777E";

/// The customer managed key limit of (`account_id`, `region`) when it is
/// enforced. Resolve it before taking the KMS state lock.
pub fn enforced_key_limit(
    provider: Option<&Arc<dyn QuotaProvider>>,
    account_id: &str,
    region: &str,
) -> Option<usize> {
    provider
        .and_then(|p| p.enforced_limit(account_id, region, SERVICE_CODE, CUSTOMER_KEYS))
        .map(|v| v.max(0.0) as usize)
}

/// Customer managed keys of `state` that live in `region`, in any key state.
pub fn customer_keys_in(state: &KmsState, region: &str) -> usize {
    state
        .keys
        .values()
        .filter(|k| k.key_manager == "CUSTOMER")
        .filter(|k| k.arn.split(':').nth(3) == Some(region))
        .count()
}

/// Refuse one more customer managed key in a Region that already holds
/// `existing` when that reaches the enforced `limit`. A `None` limit accepts
/// anything.
pub fn check_new_key(limit: Option<usize>, existing: usize) -> Result<(), AwsServiceError> {
    match limit {
        Some(limit) if existing >= limit => Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "LimitExceededException",
            format!(
                "The request was rejected because the quota of {limit} customer managed KMS \
                 keys in this Region was exceeded."
            ),
        )),
        _ => Ok(()),
    }
}

/// Counts customer managed keys per Region, so Service Quotas utilization
/// reports show real usage.
pub struct KmsQuotaUsage {
    state: SharedKmsState,
}

impl KmsQuotaUsage {
    pub fn new(state: SharedKmsState) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

impl QuotaUsageSource for KmsQuotaUsage {
    fn service_codes(&self) -> &[&str] {
        &[SERVICE_CODE]
    }

    fn usage(
        &self,
        account_id: &str,
        region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64> {
        if (service_code, quota_code) != (SERVICE_CODE, CUSTOMER_KEYS) {
            return None;
        }
        let accounts = self.state.read();
        Some(
            accounts
                .get(account_id)
                .map_or(0, |s| customer_keys_in(s, region)) as f64,
        )
    }
}
