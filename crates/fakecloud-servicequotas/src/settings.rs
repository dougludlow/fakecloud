//! Which quotas fakecloud enforces, and how increase requests are decided.
//!
//! Enforcement is opt-in. A quota is enforced when, in order of precedence:
//!
//! 1. the account has a per-quota override (`enforce` or `ignore`), else
//! 2. the server has a per-quota override, else
//! 3. the global switch (`--enforce-quotas`) is on.
//!
//! Nothing is enforced by default, so a local test suite that creates more
//! resources than a fresh AWS account allows keeps working until the user
//! asks for limit fidelity. Only quotas a fakecloud service can check
//! ([`QuotaDef::enforceable`]) are ever enforced.
//!
//! The server-wide part lives in [`QuotaSettings`]; per-account overrides live
//! in [`ServiceQuotasData::enforcement`] next to the account's applied values.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::catalog::QuotaDef;
use crate::state::ServiceQuotasData;

/// How `RequestServiceQuotaIncrease` (and template entries applied to new
/// accounts) are decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestApproval {
    /// Decided on submission: approved unless AWS would refuse the value.
    #[default]
    Auto,
    /// Left `PENDING` until approved or denied through the introspection API,
    /// so code that polls a request's status can be tested.
    Manual,
}

impl RequestApproval {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// Server-wide quota settings: the global enforcement switch, per-quota
/// overrides that apply to every account, and the request approval mode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaSettings {
    /// Enforce every enforceable quota that has no override.
    #[serde(default)]
    pub enforce_all: bool,
    /// Per-quota overrides keyed by [`quota_ref`]: `true` enforces the quota,
    /// `false` ignores it, whatever `enforce_all` says.
    #[serde(default)]
    pub overrides: BTreeMap<String, bool>,
    #[serde(default)]
    pub request_approval: RequestApproval,
}

pub type SharedQuotaSettings = Arc<RwLock<QuotaSettings>>;

/// Why a quota is or is not enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementSource {
    /// No fakecloud service checks this quota.
    NotEnforceable,
    /// The account's own override.
    AccountOverride,
    /// A server-wide override for this quota.
    Override,
    /// The global switch.
    Global,
}

impl EnforcementSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotEnforceable => "not_enforceable",
            Self::AccountOverride => "account_override",
            Self::Override => "override",
            Self::Global => "global",
        }
    }
}

/// `service_code/quota_code`, the key overrides are stored under and the form
/// the CLI flags take.
pub fn quota_ref(def: &QuotaDef) -> String {
    quota_ref_of(def.service_code, def.quota_code)
}

/// [`quota_ref`] from the codes.
pub fn quota_ref_of(service_code: &str, quota_code: &str) -> String {
    format!("{service_code}/{quota_code}")
}

/// Whether `def` is enforced for the account holding `data`, and why.
pub fn enforcement(
    settings: &QuotaSettings,
    data: Option<&ServiceQuotasData>,
    def: &QuotaDef,
) -> (bool, EnforcementSource) {
    enforcement_of(settings, data, def.service_code, def.quota_code)
}

/// [`enforcement`] by codes. Reads only the static enforceable overlay, never
/// the decoded catalog, so the request path of a service that enforces nothing
/// stays cheap.
pub fn enforcement_of(
    settings: &QuotaSettings,
    data: Option<&ServiceQuotasData>,
    service_code: &str,
    quota_code: &str,
) -> (bool, EnforcementSource) {
    if !crate::catalog::is_enforceable(service_code, quota_code) {
        return (false, EnforcementSource::NotEnforceable);
    }
    let key = quota_ref_of(service_code, quota_code);
    if let Some(on) = data.and_then(|d| d.enforcement.get(&key)) {
        return (*on, EnforcementSource::AccountOverride);
    }
    if let Some(on) = settings.overrides.get(&key) {
        return (*on, EnforcementSource::Override);
    }
    (settings.enforce_all, EnforcementSource::Global)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn nothing_is_enforced_by_default() {
        for def in catalog::quotas() {
            assert!(!enforcement(&QuotaSettings::default(), None, def).0);
        }
    }

    #[test]
    fn account_override_beats_server_override_beats_global() {
        let def = catalog::quota(catalog::VPC, catalog::RULES_PER_SECURITY_GROUP).unwrap();
        let mut settings = QuotaSettings {
            enforce_all: true,
            ..Default::default()
        };
        assert_eq!(
            enforcement(&settings, None, def),
            (true, EnforcementSource::Global)
        );
        settings.overrides.insert(quota_ref(def), false);
        assert_eq!(
            enforcement(&settings, None, def),
            (false, EnforcementSource::Override)
        );
        let mut data = ServiceQuotasData::default();
        data.enforcement.insert(quota_ref(def), true);
        assert_eq!(
            enforcement(&settings, Some(&data), def),
            (true, EnforcementSource::AccountOverride)
        );
    }

    #[test]
    fn a_quota_no_service_checks_is_never_enforced() {
        let def = catalog::quota("lambda", "L-B99A9384").unwrap();
        assert!(!def.enforceable);
        let mut settings = QuotaSettings {
            enforce_all: true,
            ..Default::default()
        };
        settings.overrides.insert(quota_ref(def), true);
        assert_eq!(
            enforcement(&settings, None, def),
            (false, EnforcementSource::NotEnforceable)
        );
    }
}
