//! The Service Quotas quota DynamoDB enforces: "Maximum number of tables"
//! (`dynamodb`/`L-F98FE922`), per account and region.
//!
//! Enforcement is opt-in: the quota is only checked once the user switched it
//! on in Service Quotas, and a `DynamoDbService` without a quota provider
//! enforces nothing. Every table in the region counts, replicas of
//! multi-region tables included (a replica is a table in its own region).

use std::sync::Arc;

use fakecloud_core::quota::{QuotaProvider, QuotaUsageSource};
use fakecloud_core::service::AwsServiceError;
use http::StatusCode;

use crate::state::SharedDynamoDbState;

/// Service code of the DynamoDB quotas.
pub const SERVICE_CODE: &str = "dynamodb";
/// "Maximum number of tables".
pub const TABLES: &str = "L-F98FE922";

/// The table limit of (`account_id`, `region`) when it is enforced. Resolve
/// it before taking the DynamoDB state lock.
pub fn enforced_table_limit(
    provider: Option<&Arc<dyn QuotaProvider>>,
    account_id: &str,
    region: &str,
) -> Option<usize> {
    provider
        .and_then(|p| p.enforced_limit(account_id, region, SERVICE_CODE, TABLES))
        .map(|v| v.max(0.0) as usize)
}

/// `LimitExceededException` for a table create past `limit`, with the
/// message DynamoDB returns when the account's table quota is reached.
pub fn table_limit_exceeded(limit: usize) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "LimitExceededException",
        format!("Subscriber limit exceeded: There is a limit of {limit} tables per subscriber"),
    )
}

/// Refuse a new table in a region that already holds `existing` tables when
/// that reaches the enforced `limit`. A `None` limit accepts anything.
pub fn check_new_table(limit: Option<usize>, existing: usize) -> Result<(), AwsServiceError> {
    match limit {
        Some(limit) if existing >= limit => Err(table_limit_exceeded(limit)),
        _ => Ok(()),
    }
}

/// Counts the tables behind the table quota, so Service Quotas utilization
/// reports show real usage.
pub struct DynamoDbQuotaUsage {
    state: SharedDynamoDbState,
}

impl DynamoDbQuotaUsage {
    pub fn new(state: SharedDynamoDbState) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

impl QuotaUsageSource for DynamoDbQuotaUsage {
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
        if (service_code, quota_code) != (SERVICE_CODE, TABLES) {
            return None;
        }
        let accounts = self.state.read();
        Some(
            accounts
                .regional(account_id, region)
                .map_or(0, |s| s.tables.len()) as f64,
        )
    }
}
