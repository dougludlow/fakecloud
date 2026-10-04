//! In-memory state for ACM certificates.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub type SharedAcmState = Arc<RwLock<AcmAccounts>>;

/// ACM state, partitioned by account and then by region.
///
/// ACM is a regional service: a certificate (and an ACME endpoint, binding,
/// domain validation or account configuration) lives in exactly one region,
/// and a request only sees the resources of the region it is sent to. A
/// certificate ARN names its region, so a lookup by ARN resolves in that
/// region and an ARN from another region is simply not found.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AcmAccounts {
    /// account id -> region -> that account's state in that region.
    pub accounts: BTreeMap<String, BTreeMap<String, AccountState>>,
}

impl AcmAccounts {
    pub fn new() -> Self {
        Self::default()
    }

    /// The state of `account_id` in `region`, `None` when nothing has
    /// touched it. Never creates anything.
    pub fn region(&self, account_id: &str, region: &str) -> Option<&AccountState> {
        self.accounts.get(account_id).and_then(|r| r.get(region))
    }

    /// The state of `account_id` in `region`, created empty on first use.
    pub fn region_mut(&mut self, account_id: &str, region: &str) -> &mut AccountState {
        self.accounts
            .entry(account_id.to_string())
            .or_default()
            .entry(region.to_string())
            .or_default()
    }

    /// The state of `account_id` in `region` without creating it.
    pub fn region_get_mut(&mut self, account_id: &str, region: &str) -> Option<&mut AccountState> {
        self.accounts
            .get_mut(account_id)
            .and_then(|r| r.get_mut(region))
    }

    /// The state of the account and region an ARN names, without creating
    /// it. `None` when the ARN carries no account or region.
    pub fn by_arn(&self, arn: &str) -> Option<&AccountState> {
        let account = fakecloud_aws::arn::account_of(arn)?;
        let region = fakecloud_aws::arn::region_of(arn)?;
        self.region(account, region)
    }

    /// Mutable [`Self::by_arn`].
    pub fn by_arn_mut(&mut self, arn: &str) -> Option<&mut AccountState> {
        let account = fakecloud_aws::arn::account_of(arn)?.to_string();
        let region = fakecloud_aws::arn::region_of(arn)?.to_string();
        self.region_get_mut(&account, &region)
    }

    /// Every (account, region, state) triple.
    pub fn iter_regional(&self) -> impl Iterator<Item = (&str, &str, &AccountState)> {
        self.accounts.iter().flat_map(|(account, regions)| {
            regions
                .iter()
                .map(move |(region, s)| (account.as_str(), region.as_str(), s))
        })
    }

    /// Every (account, region, state) triple (mutable).
    pub fn iter_regional_mut(&mut self) -> impl Iterator<Item = (&str, &str, &mut AccountState)> {
        self.accounts.iter_mut().flat_map(|(account, regions)| {
            regions
                .iter_mut()
                .map(move |(region, s)| (account.as_str(), region.as_str(), s))
        })
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccountState {
    /// Keyed by full certificate ARN.
    pub certificates: BTreeMap<String, StoredCertificate>,
    pub account_config: AccountConfig,
    /// ACME endpoints keyed by `AcmeEndpointArn`.
    #[serde(default)]
    pub acme_endpoints: BTreeMap<String, AcmeEndpoint>,
    /// External account bindings keyed by `AcmeExternalAccountBindingArn`.
    #[serde(default)]
    pub acme_bindings: BTreeMap<String, AcmeBinding>,
    /// Domain validations keyed by `AcmeDomainValidationArn`.
    #[serde(default)]
    pub acme_domain_validations: BTreeMap<String, AcmeDomainValidation>,
    /// ACME accounts keyed by `(endpoint arn, account url)`.
    #[serde(default)]
    pub acme_accounts: BTreeMap<String, AcmeAccount>,
}

impl AccountState {
    /// The tag set of any taggable ACM resource (certificate, ACME endpoint,
    /// external account binding, or domain validation) by its ARN.
    pub fn resource_tags(&self, arn: &str) -> Option<&BTreeMap<String, String>> {
        if let Some(c) = self.certificates.get(arn) {
            return Some(&c.tags);
        }
        if let Some(e) = self.acme_endpoints.get(arn) {
            return Some(&e.tags);
        }
        if let Some(b) = self.acme_bindings.get(arn) {
            return Some(&b.tags);
        }
        self.acme_domain_validations.get(arn).map(|d| &d.tags)
    }

    /// Mutable counterpart of [`AccountState::resource_tags`].
    pub fn resource_tags_mut(&mut self, arn: &str) -> Option<&mut BTreeMap<String, String>> {
        if let Some(c) = self.certificates.get_mut(arn) {
            return Some(&mut c.tags);
        }
        if let Some(e) = self.acme_endpoints.get_mut(arn) {
            return Some(&mut e.tags);
        }
        if let Some(b) = self.acme_bindings.get_mut(arn) {
            return Some(&mut b.tags);
        }
        self.acme_domain_validations
            .get_mut(arn)
            .map(|d| &mut d.tags)
    }
}

/// An ACME endpoint: the directory a client talks to, plus the CA it issues
/// from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeEndpoint {
    pub arn: String,
    pub endpoint_url: String,
    /// `AcmeEndpointStatus` (CREATING | ACTIVE | DELETING | FAILED).
    pub status: String,
    pub authorization_behavior: String,
    pub contact: Option<String>,
    /// The `CertificateAuthority` union, stored as supplied.
    pub certificate_authority: serde_json::Value,
    pub certificate_tags: BTreeMap<String, String>,
    pub tags: BTreeMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set by the caller's `IdempotencyToken`, so a repeat create returns the
    /// same endpoint rather than a second one.
    pub idempotency_token: Option<String>,
}

/// An external account binding: the HMAC credential an ACME client uses to
/// bind its account to this AWS account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeBinding {
    pub arn: String,
    pub endpoint_arn: String,
    pub role_arn: String,
    /// The HMAC key id and secret returned by
    /// `GetAcmeExternalAccountBindingCredentials`.
    pub key_id: String,
    pub mac_key: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tags: BTreeMap<String, String>,
    pub idempotency_token: Option<String>,
}

/// The three independent scope options of a DNS prevalidation, each
/// `ENABLED` or `DISABLED`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DomainScope {
    #[serde(default)]
    pub exact_domain: Option<String>,
    #[serde(default)]
    pub subdomains: Option<String>,
    #[serde(default)]
    pub wildcards: Option<String>,
}

/// A pre-validated domain: the DNS record an ACME client can rely on instead
/// of answering a challenge per order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeDomainValidation {
    pub arn: String,
    pub endpoint_arn: String,
    pub domain_name: String,
    /// `PrevalidationType` — only DNS_PREVALIDATION exists today.
    pub prevalidation_type: String,
    /// `DomainScope`: which names the prevalidation covers. Modeled as a
    /// structure of three `ENABLED`/`DISABLED` options, not a single enum.
    #[serde(default)]
    pub domain_scope: Option<DomainScope>,
    pub hosted_zone_id: Option<String>,
    /// The CNAME an operator publishes to prove control.
    pub record_name: String,
    pub record_value: String,
    /// `AcmeDomainValidationStatus`.
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tags: BTreeMap<String, String>,
    pub idempotency_token: Option<String>,
}

/// An ACME account registered against an endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeAccount {
    pub endpoint_arn: String,
    pub account_url: String,
    pub public_key_thumbprint: String,
    /// `AcmeAccountStatus` (VALID | DEACTIVATED | REVOKED).
    pub status: String,
    pub binding_arn: Option<String>,
    pub contacts: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountConfig {
    pub expiry_events_days_before_expiry: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCertificate {
    pub arn: String,
    pub domain_name: String,
    pub subject_alternative_names: Vec<String>,
    pub status: String,
    pub cert_type: String,
    /// Stored when present so we can round-trip it on `GetCertificate`.
    pub certificate_pem: Option<String>,
    pub certificate_chain_pem: Option<String>,
    /// Imported certs only — held in memory but never returned
    /// (matches real ACM, which never returns the private key).
    pub private_key_pem: Option<String>,
    pub idempotency_token: Option<String>,
    pub serial: String,
    pub subject: String,
    pub issuer: String,
    pub key_algorithm: String,
    pub signature_algorithm: String,
    pub created_at: DateTime<Utc>,
    pub issued_at: Option<DateTime<Utc>>,
    pub imported_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revocation_reason: Option<String>,
    /// Last reason recorded by the admin status mutator when the cert
    /// is flipped to `FAILED` / `VALIDATION_TIMED_OUT`. Surfaced in
    /// `DescribeCertificate` as `FailureReason` to match real ACM.
    #[serde(default)]
    pub failure_reason: Option<String>,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub validation_method: Option<String>,
    pub domain_validation: Vec<DomainValidation>,
    pub options: CertificateOptions,
    pub renewal_eligibility: String,
    pub managed_by: Option<String>,
    pub certificate_authority_arn: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub in_use_by: Vec<String>,
    /// Number of `DescribeCertificate` reads since the cert was issued.
    /// Legacy field kept for state-file compatibility; the read-count
    /// flip was removed in favour of the async auto-issue tick (see
    /// `AcmService::pending_validation_delay`).
    #[serde(default)]
    pub describe_read_count: u32,
    /// Snapshot of the last managed-renewal round. `None` until either
    /// the auto-issue tick fires (for DNS) or the admin `/approve`
    /// endpoint flips an EMAIL cert; refreshed on every successful
    /// `RenewCertificate`. Surfaced as `RenewalSummary` in
    /// `DescribeCertificate` for `AMAZON_ISSUED` certs.
    #[serde(default)]
    pub renewal_summary: Option<RenewalSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewalSummary {
    /// One of `PENDING_AUTO_RENEWAL`, `PENDING_VALIDATION`, `SUCCESS`, `FAILED`.
    pub renewal_status: String,
    /// Per-domain validation snapshot at the moment the renewal summary
    /// was emitted. fakecloud copies the cert's current
    /// `domain_validation` into this field so callers see consistent
    /// data between top-level `DomainValidationOptions` and
    /// `RenewalSummary.DomainValidationOptions`.
    pub domain_validation: Vec<DomainValidation>,
    /// Optional renewal failure reason. Real ACM uses
    /// `RenewalStatusReason` (an enum: `NO_AVAILABLE_CONTACTS`,
    /// `ADDITIONAL_VERIFICATION_REQUIRED`, etc.); fakecloud just stores
    /// whatever string the admin endpoint or renew flow recorded.
    pub renewal_status_reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainValidation {
    pub domain_name: String,
    pub validation_status: String,
    pub validation_method: String,
    pub resource_record_name: Option<String>,
    pub resource_record_type: Option<String>,
    pub resource_record_value: Option<String>,
    /// HTTP validation redirect (`HttpRedirect.RedirectFrom`) — set for
    /// ValidationMethod=HTTP certificates.
    #[serde(default)]
    pub http_redirect_from: Option<String>,
    /// HTTP validation redirect target (`HttpRedirect.RedirectTo`).
    #[serde(default)]
    pub http_redirect_to: Option<String>,
    /// Domain the EMAIL challenge is sent to (`DomainValidationOptions[].
    /// ValidationDomain`): the domain itself or a superdomain of it. `None`
    /// for DNS and HTTP validation.
    #[serde(default)]
    pub validation_domain: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CertificateOptions {
    pub certificate_transparency_logging_preference: String,
    pub export: String,
}

/// On-disk snapshot envelope for ACM state. Versioned so format changes fail
/// loudly on upgrade rather than silently mis-parsing.
#[derive(Clone, Serialize, Deserialize)]
pub struct AcmSnapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub accounts: Option<AcmAccounts>,
}

/// Bumped to 2 when the ACME resources landed. An older binary reading a
/// snapshot that carries them would drop the unknown maps silently, so the
/// version guard has to reject the downgrade rather than lose state.
/// Bumped to 3 when the state was split by region: v1/v2 kept one
/// account-wide state, migrated on load by [`parse_acm_snapshot`].
pub const ACM_SNAPSHOT_SCHEMA_VERSION: u32 = 3;

#[derive(Deserialize)]
struct SnapshotVersion {
    schema_version: u32,
}

/// The v1/v2 snapshot shape: one account-wide state per account.
#[derive(Deserialize)]
struct LegacyAcmSnapshot {
    #[serde(default)]
    accounts: Option<LegacyAcmAccounts>,
}

#[derive(Deserialize)]
struct LegacyAcmAccounts {
    #[serde(default)]
    accounts: BTreeMap<String, AccountState>,
}

/// Parse an on-disk ACM snapshot, migrating the pre-region (v1/v2) shape.
///
/// A snapshot newer than this binary understands is returned with its
/// version and no state so the caller can refuse it. Legacy state is split
/// by region: every certificate, ACME endpoint, binding and domain
/// validation goes to the region its ARN names, an ACME account follows its
/// endpoint, and the account configuration (which named no region) goes to
/// `default_region`, as does any record whose ARN names none.
pub fn parse_acm_snapshot(
    bytes: &[u8],
    default_region: &str,
) -> Result<AcmSnapshot, serde_json::Error> {
    let SnapshotVersion { schema_version } = serde_json::from_slice(bytes)?;
    if schema_version >= ACM_SNAPSHOT_SCHEMA_VERSION {
        if schema_version > ACM_SNAPSHOT_SCHEMA_VERSION {
            return Ok(AcmSnapshot {
                schema_version,
                accounts: None,
            });
        }
        return serde_json::from_slice(bytes);
    }
    let legacy: LegacyAcmSnapshot = serde_json::from_slice(bytes)?;
    let accounts = legacy.accounts.map(|legacy| {
        let mut out = AcmAccounts::new();
        for (account_id, state) in legacy.accounts {
            split_legacy_account(&mut out, &account_id, state, default_region);
        }
        out
    });
    Ok(AcmSnapshot {
        schema_version: ACM_SNAPSHOT_SCHEMA_VERSION,
        accounts,
    })
}

fn split_legacy_account(
    out: &mut AcmAccounts,
    account_id: &str,
    legacy: AccountState,
    default_region: &str,
) {
    let region_of = |arn: &str| -> String {
        fakecloud_aws::arn::region_of(arn)
            .unwrap_or(default_region)
            .to_string()
    };
    let AccountState {
        certificates,
        account_config,
        acme_endpoints,
        acme_bindings,
        acme_domain_validations,
        acme_accounts,
    } = legacy;
    // The account configuration was applied to every region before; keep it
    // where requests without a region override land.
    out.region_mut(account_id, default_region).account_config = account_config;
    for (arn, cert) in certificates {
        out.region_mut(account_id, &region_of(&arn))
            .certificates
            .insert(arn, cert);
    }
    for (arn, endpoint) in acme_endpoints {
        out.region_mut(account_id, &region_of(&arn))
            .acme_endpoints
            .insert(arn, endpoint);
    }
    for (arn, binding) in acme_bindings {
        out.region_mut(account_id, &region_of(&arn))
            .acme_bindings
            .insert(arn, binding);
    }
    for (arn, validation) in acme_domain_validations {
        out.region_mut(account_id, &region_of(&arn))
            .acme_domain_validations
            .insert(arn, validation);
    }
    for (key, account) in acme_accounts {
        out.region_mut(account_id, &region_of(&account.endpoint_arn))
            .acme_accounts
            .insert(key, account);
    }
}
