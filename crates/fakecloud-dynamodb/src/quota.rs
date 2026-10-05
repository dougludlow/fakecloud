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

use fakecloud_core::multi_account::MultiRegionState;

use crate::state::{DynamoDbState, SharedDynamoDbState};

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
    fakecloud_core::quota::enforced_count(provider, account_id, region, SERVICE_CODE, TABLES)
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

/// Refuse a new table in the region `state` holds when it already has as many
/// tables as the enforced `limit`. Every path that creates a table (the API
/// creates and restores, imports, CloudFormation) goes through here. A `None`
/// limit accepts anything.
pub fn check_new_table(state: &DynamoDbState, limit: Option<usize>) -> Result<(), AwsServiceError> {
    check_room(limit, state.tables.len())
}

fn check_room(limit: Option<usize>, existing: usize) -> Result<(), AwsServiceError> {
    if fakecloud_core::quota::has_room(limit, existing) {
        return Ok(());
    }
    Err(table_limit_exceeded(limit.unwrap_or_default()))
}

/// Refuse new replicas of the table named `table_name` that would take a
/// region past its enforced table limit. `limits` holds each target region
/// with its limit, resolved before the DynamoDB lock; a region that already
/// holds a table of that name gets no new table there. `table_name` is `None`
/// for a table whose name is not decided yet, which no region holds. Used by
/// `UpdateTable` `ReplicaUpdates` and CloudFormation global tables alike.
pub fn check_new_replicas(
    accounts: &MultiRegionState<DynamoDbState>,
    account_id: &str,
    table_name: Option<&str>,
    limits: &[(String, Option<usize>)],
) -> Result<(), AwsServiceError> {
    for (region, limit) in limits {
        let state = accounts.regional(account_id, region);
        let holds_table =
            table_name.is_some_and(|name| state.is_some_and(|s| s.tables.contains_key(name)));
        if !holds_table {
            check_room(*limit, state.map_or(0, |s| s.tables.len()))?;
        }
    }
    Ok(())
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
