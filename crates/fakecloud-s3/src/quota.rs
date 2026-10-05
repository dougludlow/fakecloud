//! The Service Quotas quota S3 enforces: "General purpose buckets"
//! (`s3`/`L-DC2B2D3D`).
//!
//! The quota is per account across all Regions, and S3 manages it from one
//! Region of the partition: US East (N. Virginia) for the commercial
//! partition and AWS GovCloud (US-West) for GovCloud (the partition's primary
//! Region, likewise in the other partitions). fakecloud reads its applied
//! value from that Region whichever Region the bucket is created in.
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

/// The Region the bucket quota of `region`'s partition is managed from.
pub fn quota_region(region: &str) -> &'static str {
    fakecloud_aws::arn::partition_primary_region(region)
}

/// The bucket limit of `account_id` when it is enforced. Resolve it before
/// taking the S3 state lock.
pub fn enforced_bucket_limit(
    provider: Option<&Arc<dyn QuotaProvider>>,
    account_id: &str,
    region: &str,
) -> Option<usize> {
    fakecloud_core::quota::enforced_count(
        provider,
        account_id,
        quota_region(region),
        SERVICE_CODE,
        BUCKETS,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quota_lives_in_the_partitions_home_region() {
        assert_eq!(quota_region("eu-west-1"), "us-east-1");
        assert_eq!(quota_region("us-east-1"), "us-east-1");
        assert_eq!(quota_region("us-gov-east-1"), "us-gov-west-1");
        assert_eq!(quota_region("cn-northwest-1"), "cn-north-1");
        assert_eq!(quota_region("us-iso-west-1"), "us-iso-east-1");
    }
}
