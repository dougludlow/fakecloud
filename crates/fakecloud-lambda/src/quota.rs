//! The Service Quotas quota Lambda enforces: "Function and layer storage"
//! (`lambda`/`L-2ACBD22F`, in gigabytes), per account and Region.
//!
//! Lambda keeps a copy of every deployment package it stores: the code of
//! each function's `$LATEST`, of every published version, and of every layer
//! version. Their sizes add up to the storage the quota caps (container-image
//! functions keep their code in ECR and add nothing). A deploy that would take
//! the total past the applied value fails with `CodeStorageExceededException`.
//! Enforcement is opt-in: the quota is only checked once the user switched it
//! on in Service Quotas, and a `LambdaService` without a quota provider
//! enforces nothing.
//!
//! "Concurrent executions" (`L-B99A9384`) is not enforced: cross-service
//! invocations (event source mappings, SNS, S3, EventBridge, ...) run the
//! function without going through the `Invoke` concurrency gate, so the
//! account-wide count of in-flight executions cannot be measured faithfully.
//! Its applied value is still the account limit `GetAccountSettings` reports
//! and `PutFunctionConcurrency` keeps the unreserved minimum against.

use std::sync::Arc;

use fakecloud_core::quota::{QuotaProvider, QuotaUsageSource};
use fakecloud_core::service::AwsServiceError;
use http::StatusCode;

use crate::state::{LambdaFunction, LambdaState, SharedLambdaState};

/// Service code of the Lambda quotas.
pub const SERVICE_CODE: &str = "lambda";
/// "Function and layer storage", in gigabytes.
pub const CODE_STORAGE: &str = "L-2ACBD22F";

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The code storage limit of (`account_id`, `region`) in bytes when it is
/// enforced. Resolve it before taking the Lambda state lock.
pub fn enforced_storage_limit(
    provider: Option<&Arc<dyn QuotaProvider>>,
    account_id: &str,
    region: &str,
) -> Option<i64> {
    provider
        .and_then(|p| p.enforced_limit(account_id, region, SERVICE_CODE, CODE_STORAGE))
        .map(|gb| (gb.max(0.0) * BYTES_PER_GB) as i64)
}

/// The bytes of code a function record stores: its package size, or nothing
/// for a container image.
pub fn stored_code_size(func: &LambdaFunction) -> i64 {
    if func.package_type == "Image" {
        0
    } else {
        func.code_size.max(0)
    }
}

/// Code storage `state` uses: every function's `$LATEST`, every published
/// version and every layer version.
pub fn code_storage_used(state: &LambdaState) -> i64 {
    let functions: i64 = state.functions.values().map(stored_code_size).sum();
    let versions: i64 = state
        .function_version_snapshots
        .values()
        .flat_map(|versions| versions.values())
        .map(stored_code_size)
        .sum();
    let layers: i64 = state
        .layers
        .values()
        .flat_map(|l| l.versions.iter())
        .map(|v| v.code_size.max(0))
        .sum();
    functions + versions + layers
}

/// Refuse a deploy that grows code storage from `before` to `after` bytes
/// past the enforced `limit`. A `None` limit, or a deploy that does not grow
/// storage, is accepted.
pub fn check_storage(limit: Option<i64>, before: i64, after: i64) -> Result<(), AwsServiceError> {
    match limit {
        Some(limit) if after > limit && after > before => Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "CodeStorageExceededException",
            "Code storage limit exceeded.",
        )),
        _ => Ok(()),
    }
}

/// "Concurrent executions", the account concurrency limit.
pub const CONCURRENT_EXECUTIONS: &str = "L-B99A9384";

/// The concurrency limit of `state`'s Region: the stored account settings,
/// else `applied` (the Service Quotas value, when attached), else the AWS
/// default of 1000.
pub fn concurrency_limit(state: &LambdaState, applied: Option<f64>) -> i64 {
    state
        .account_settings
        .as_ref()
        .map(|s| s.concurrent_executions)
        .or_else(|| applied.map(|v| v.max(0.0) as i64))
        .unwrap_or(1000)
}

/// Lambda always leaves part of the account's concurrency unreserved for the
/// functions without reserved concurrency: `PutFunctionConcurrency` refuses
/// a reservation that takes the unreserved pool below 100 (or below the
/// whole limit, for an account whose limit is under 100).
pub fn check_reservation(
    state: &LambdaState,
    function_name: &str,
    requested: i64,
    limit: i64,
) -> Result<(), AwsServiceError> {
    let others: i64 = state
        .function_concurrency
        .iter()
        .filter(|(name, _)| name.as_str() != function_name)
        .map(|(_, n)| *n)
        .sum();
    let minimum = limit.min(100);
    if limit - others - requested < minimum {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "InvalidParameterValueException",
            format!(
                "Specified ReservedConcurrentExecutions for function decreases account's \
                 UnreservedConcurrentExecution below its minimum value of [{minimum}]."
            ),
        ));
    }
    Ok(())
}

/// Reports code storage in gigabytes, the quota's unit, so Service Quotas
/// utilization reports show real usage.
pub struct LambdaQuotaUsage {
    state: SharedLambdaState,
}

impl LambdaQuotaUsage {
    pub fn new(state: SharedLambdaState) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

impl QuotaUsageSource for LambdaQuotaUsage {
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
        if (service_code, quota_code) != (SERVICE_CODE, CODE_STORAGE) {
            return None;
        }
        let accounts = self.state.read();
        let bytes = accounts
            .regional(account_id, region)
            .map_or(0, code_storage_used);
        Some(bytes as f64 / BYTES_PER_GB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_is_checked_only_when_it_grows_past_the_limit() {
        assert!(check_storage(None, 0, i64::MAX).is_ok());
        assert!(check_storage(Some(10), 0, 10).is_ok());
        let err = check_storage(Some(10), 0, 11).err().unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.code(), "CodeStorageExceededException");
        assert_eq!(err.message(), "Code storage limit exceeded.");
        // Shrinking an over-limit account is not refused.
        assert!(check_storage(Some(10), 20, 15).is_ok());
    }
}
