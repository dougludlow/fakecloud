//! The Service Quotas quota S3 enforces: "General purpose buckets"
//! (`s3`/`L-DC2B2D3D`).
//!
//! The quota is per account across all Regions: Service Quotas lists it as a
//! global quota, so it has one applied value whichever Region it is read or
//! raised in, and every bucket the account owns counts toward it.
//! Enforcement is opt-in: the quota is only checked once the user switched it
//! on in Service Quotas, and an `S3Service` without a quota provider enforces
//! nothing.

use std::sync::Arc;

use fakecloud_core::quota::{QuotaProvider, QuotaUsageSource};
use fakecloud_core::service::AwsServiceError;
use http::StatusCode;

use crate::state::{S3State, SharedS3State};

/// Service code of the S3 quotas.
pub const SERVICE_CODE: &str = "s3";
/// "General purpose buckets".
pub const BUCKETS: &str = "L-DC2B2D3D";

/// The bucket limit of `account_id` when it is enforced. Resolve it before
/// taking the S3 state lock.
pub fn enforced_bucket_limit(
    provider: Option<&Arc<dyn QuotaProvider>>,
    account_id: &str,
    region: &str,
) -> Option<usize> {
    fakecloud_core::quota::enforced_count(provider, account_id, region, SERVICE_CODE, BUCKETS)
}

/// Refuse one more bucket for the account `state` holds when it already owns
/// as many as the enforced `limit`. Both `CreateBucket` and a CloudFormation
/// bucket go through here. A `None` limit accepts anything.
pub fn check_new_bucket(state: &S3State, limit: Option<usize>) -> Result<(), AwsServiceError> {
    if fakecloud_core::quota::has_room(limit, state.buckets.len()) {
        return Ok(());
    }
    Err(AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "TooManyBuckets",
        "You have attempted to create more buckets than allowed",
    ))
}

/// Counts an account's buckets (all Regions), so Service Quotas utilization
/// reports show real usage.
pub struct S3QuotaUsage {
    state: SharedS3State,
}

impl S3QuotaUsage {
    pub fn new(state: SharedS3State) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

impl QuotaUsageSource for S3QuotaUsage {
    fn service_codes(&self) -> &[&str] {
        &[SERVICE_CODE]
    }

    fn usage(
        &self,
        account_id: &str,
        _region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64> {
        if (service_code, quota_code) != (SERVICE_CODE, BUCKETS) {
            return None;
        }
        let accounts = self.state.read();
        Some(accounts.get(account_id).map_or(0, |s| s.buckets.len()) as f64)
    }
}
