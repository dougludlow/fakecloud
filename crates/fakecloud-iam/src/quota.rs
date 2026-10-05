//! The Service Quotas quotas IAM enforces.
//!
//! Enforcement is opt-in: a quota is only checked once the user switched it
//! on in Service Quotas, and an `IamService` without a quota provider
//! enforces nothing. IAM quotas are global, so the request region only
//! picks the partition. A refused request gets IAM's `LimitExceeded` (HTTP
//! 409) with AWS's `Cannot exceed quota for <Name>: <limit>` message, where
//! `<Name>` is the internal quota name IAM reports for that limit.

use std::sync::Arc;

use fakecloud_core::quota::{QuotaProvider, QuotaUsageSource, IAM_SERVICE_CODE};
use fakecloud_core::service::AwsServiceError;
use http::StatusCode;

use crate::state::{IamState, SharedIamState};

/// An IAM quota fakecloud can enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IamQuota {
    Users,
    Roles,
    Groups,
    CustomerManagedPolicies,
    ManagedPoliciesPerRole,
    ManagedPoliciesPerUser,
    ManagedPoliciesPerGroup,
    ServerCertificates,
    OpenIdConnectProviders,
    InstanceProfiles,
    RoleTrustPolicyLength,
}

impl IamQuota {
    /// The Service Quotas quota code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Users => "L-F55AF5E4",
            Self::Roles => "L-FE177D64",
            Self::Groups => "L-F4A5425F",
            Self::CustomerManagedPolicies => "L-E95E4862",
            Self::ManagedPoliciesPerRole => "L-0DA4ABF3",
            Self::ManagedPoliciesPerUser => "L-4019AD8B",
            Self::ManagedPoliciesPerGroup => "L-384571C4",
            Self::ServerCertificates => "L-BF35879D",
            Self::OpenIdConnectProviders => "L-858F3967",
            Self::InstanceProfiles => "L-6E65F664",
            Self::RoleTrustPolicyLength => "L-C07B4B0D",
        }
    }

    /// The quota name IAM puts in its `LimitExceeded` message.
    fn error_name(self) -> &'static str {
        match self {
            Self::Users => "UsersPerAccount",
            Self::Roles => "RolesPerAccount",
            Self::Groups => "GroupsPerAccount",
            Self::CustomerManagedPolicies => "PoliciesPerAccount",
            Self::ManagedPoliciesPerRole => "PoliciesPerRole",
            Self::ManagedPoliciesPerUser => "PoliciesPerUser",
            Self::ManagedPoliciesPerGroup => "PoliciesPerGroup",
            Self::ServerCertificates => "ServerCertificatesPerAccount",
            Self::OpenIdConnectProviders => "OpenIdConnectProvidersPerAccount",
            Self::InstanceProfiles => "InstanceProfilesPerAccount",
            Self::RoleTrustPolicyLength => "ACLSizePerRole",
        }
    }
}

/// The limit of `quota` for `account_id` when it is enforced, `None` when it
/// is not (no provider, or enforcement is off for it). Resolve it before
/// taking the IAM state lock: the provider takes Service Quotas' locks.
pub fn enforced_limit(
    provider: Option<&Arc<dyn QuotaProvider>>,
    account_id: &str,
    region: &str,
    quota: IamQuota,
) -> Option<usize> {
    fakecloud_core::quota::enforced_count(
        provider,
        account_id,
        region,
        IAM_SERVICE_CODE,
        quota.code(),
    )
}

/// `LimitExceeded` for `quota` at `limit`.
pub fn limit_exceeded(quota: IamQuota, limit: usize) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::CONFLICT,
        "LimitExceeded",
        format!("Cannot exceed quota for {}: {limit}", quota.error_name()),
    )
}

/// Refuse a request that would take `quota` to `count_after` when that is
/// past the enforced `limit`. A `None` limit accepts anything.
pub fn check(
    quota: IamQuota,
    limit: Option<usize>,
    count_after: usize,
) -> Result<(), AwsServiceError> {
    match limit {
        Some(limit) if count_after > limit => Err(limit_exceeded(quota, limit)),
        _ => Ok(()),
    }
}

/// Refuse one more entity of an account-level `quota` (users, roles, groups,
/// customer managed policies, instance profiles, server certificates, OIDC
/// providers) when `state` already holds as many as the enforced `limit`.
/// The IAM API and CloudFormation both create through here. A `None` limit
/// accepts anything.
pub fn check_new(
    state: &IamState,
    quota: IamQuota,
    limit: Option<usize>,
) -> Result<(), AwsServiceError> {
    if limit.is_none() {
        return Ok(());
    }
    check(quota, limit, account_usage(state, quota).unwrap_or(0) + 1)
}

/// The managed policies a principal holds after attaching `adding` to
/// `current`, counting a policy already attached (or listed twice) once.
pub fn attached_after(current: &[String], adding: &[String]) -> usize {
    let mut all: Vec<&String> = current.iter().collect();
    for a in adding {
        if !all.contains(&a) {
            all.push(a);
        }
    }
    all.len()
}

/// Refuse attaching `adding` to a principal holding `current` when that takes
/// it past the enforced per-principal `quota` (managed policies per role,
/// user or group). A `None` limit accepts anything.
pub fn check_attachments(
    quota: IamQuota,
    limit: Option<usize>,
    current: &[String],
    adding: &[String],
) -> Result<(), AwsServiceError> {
    if limit.is_none() {
        return Ok(());
    }
    check(quota, limit, attached_after(current, adding))
}

/// The size of a trust policy as IAM counts it against the role trust policy
/// length quota: characters, not counting white space.
pub fn trust_policy_size(document: &str) -> usize {
    document.chars().filter(|c| !c.is_whitespace()).count()
}

/// Refuse a trust policy longer than the enforced role trust policy length.
pub fn check_trust_policy(limit: Option<usize>, document: &str) -> Result<(), AwsServiceError> {
    check(
        IamQuota::RoleTrustPolicyLength,
        limit,
        trust_policy_size(document),
    )
}

/// Account-level usage of `quota` in `state`, `None` for per-entity quotas
/// (policies per role, trust policy length), which have no account usage.
pub fn account_usage(state: &IamState, quota: IamQuota) -> Option<usize> {
    Some(match quota {
        IamQuota::Users => state.users.len(),
        IamQuota::Roles => state.roles.len(),
        IamQuota::Groups => state.groups.len(),
        IamQuota::CustomerManagedPolicies => state.policies.len(),
        IamQuota::ServerCertificates => state.server_certificates.len(),
        IamQuota::OpenIdConnectProviders => state.oidc_providers.len(),
        IamQuota::InstanceProfiles => state.instance_profiles.len(),
        IamQuota::ManagedPoliciesPerRole
        | IamQuota::ManagedPoliciesPerUser
        | IamQuota::ManagedPoliciesPerGroup
        | IamQuota::RoleTrustPolicyLength => return None,
    })
}

const ACCOUNT_QUOTAS: &[IamQuota] = &[
    IamQuota::Users,
    IamQuota::Roles,
    IamQuota::Groups,
    IamQuota::CustomerManagedPolicies,
    IamQuota::ServerCertificates,
    IamQuota::OpenIdConnectProviders,
    IamQuota::InstanceProfiles,
];

/// Counts the IAM entities behind the account-level IAM quotas, so Service
/// Quotas utilization reports show real usage.
pub struct IamQuotaUsage {
    state: SharedIamState,
}

impl IamQuotaUsage {
    pub fn new(state: SharedIamState) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

impl QuotaUsageSource for IamQuotaUsage {
    fn service_codes(&self) -> &[&str] {
        &[IAM_SERVICE_CODE]
    }

    fn usage(
        &self,
        account_id: &str,
        _region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64> {
        if service_code != IAM_SERVICE_CODE {
            return None;
        }
        let quota = ACCOUNT_QUOTAS.iter().find(|q| q.code() == quota_code)?;
        let accounts = self.state.read();
        Some(
            accounts
                .get(account_id)
                .and_then(|s| account_usage(s, *quota))
                .unwrap_or(0) as f64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_iam_quota_and_limit() {
        let err = limit_exceeded(IamQuota::Roles, 3);
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(err.code(), "LimitExceeded");
        assert_eq!(err.message(), "Cannot exceed quota for RolesPerAccount: 3");
        assert_eq!(
            limit_exceeded(IamQuota::RoleTrustPolicyLength, 2048).message(),
            "Cannot exceed quota for ACLSizePerRole: 2048"
        );
    }

    #[test]
    fn check_refuses_only_past_the_limit() {
        assert!(check(IamQuota::Users, None, 10_000).is_ok());
        assert!(check(IamQuota::Users, Some(2), 2).is_ok());
        assert!(check(IamQuota::Users, Some(2), 3).is_err());
    }

    #[test]
    fn attached_after_counts_a_repeated_policy_once() {
        let current = vec!["a".to_string(), "b".to_string()];
        assert_eq!(attached_after(&current, &["b".into(), "c".into()]), 3);
        assert_eq!(attached_after(&[], &["a".into(), "a".into()]), 1);
        assert!(check_attachments(
            IamQuota::ManagedPoliciesPerRole,
            Some(2),
            &current,
            &["a".into()]
        )
        .is_ok());
        assert!(check_attachments(
            IamQuota::ManagedPoliciesPerRole,
            Some(2),
            &current,
            &["c".into()]
        )
        .is_err());
    }

    #[test]
    fn trust_policy_size_ignores_white_space() {
        assert_eq!(trust_policy_size("{ \"a\" :\n\t1 }"), 7);
        assert!(check_trust_policy(Some(7), "{ \"a\" :\n\t1 }").is_ok());
        assert!(check_trust_policy(Some(6), "{ \"a\" :\n\t1 }").is_err());
    }
}
