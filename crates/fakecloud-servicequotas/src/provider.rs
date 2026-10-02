//! Applied-value resolution shared by the Service Quotas API and the
//! [`QuotaProvider`] other services enforce quotas through.

use chrono::{DateTime, Utc};

use fakecloud_core::quota::QuotaProvider;
use fakecloud_organizations::SharedOrganizationsState;

use crate::catalog::{self, QuotaDef};
use crate::state::{applied_key, QuotaRequest, ServiceQuotasData, SharedServiceQuotasState};

/// The applied value of `def` for an account: the value an approved increase
/// raised it to, else the AWS default.
pub fn applied_value(data: Option<&ServiceQuotasData>, region: &str, def: &QuotaDef) -> f64 {
    data.and_then(|d| {
        d.applied
            .get(&applied_key(
                region,
                def.global,
                def.service_code,
                def.quota_code,
            ))
            .copied()
    })
    .unwrap_or(def.default)
}

/// The region a quota's ARN and applied value live in: global quotas have
/// none.
pub fn quota_region(region: &str, def: &QuotaDef) -> String {
    if def.global {
        String::new()
    } else {
        region.to_string()
    }
}

/// `arn:<partition>:servicequotas:<region>:<account>:<service>/<quota>`. The
/// region is empty for a global quota and the account is empty for an AWS
/// default (not applied) quota.
pub fn quota_arn(region: &str, account_id: &str, def: &QuotaDef) -> String {
    let resource = format!("{}/{}", def.service_code, def.quota_code);
    if def.global {
        fakecloud_aws::arn::Arn::global_in(region, "servicequotas", account_id, &resource)
            .to_string()
    } else {
        fakecloud_aws::arn::Arn::regional("servicequotas", region, account_id, &resource)
            .to_string()
    }
}

/// The JSON `Requester` AWS reports on a quota request.
pub fn requester(account_id: &str, caller_arn: &str) -> String {
    serde_json::json!({ "accountId": account_id, "callerArn": caller_arn }).to_string()
}

/// A Service Quotas request id: 32 hex characters followed by 8 alphanumerics,
/// the 40-character shape AWS returns.
pub fn new_request_id() -> String {
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    let suffix: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect();
    format!("{}{suffix}", uuid::Uuid::new_v4().simple())
}

/// Apply the organization's quota request template to `account_id` if the
/// account was created in the organization after the template was
/// associated, exactly once per association.
///
/// AWS applies an associated template to every *new* account created in the
/// organization by submitting each entry as a quota increase request. Doing it
/// on the account's first Service Quotas touch (an API call, or another
/// service enforcing a quota) gives the same observable result: by the time
/// anything reads the account's quotas, the template has been applied and the
/// requests are in its history.
pub fn apply_template_if_new(
    state: &SharedServiceQuotasState,
    orgs: &SharedOrganizationsState,
    account_id: &str,
    now: DateTime<Utc>,
) {
    let (management, joined) = {
        let registry = orgs.read();
        let Some(org) = registry.org_of_account(account_id) else {
            return;
        };
        if org.management_account_id == account_id {
            return;
        }
        let joined = org
            .accounts
            .get(account_id)
            .filter(|a| a.joined_method == "CREATED")
            .map(|a| a.joined_timestamp);
        (org.management_account_id.clone(), joined)
    };

    // Cheap read-locked check first so the common case never takes the write
    // lock.
    let (marker, entries) = {
        let guard = state.read();
        let Some(mgmt) = guard.get(&management) else {
            return;
        };
        let Some(associated_at) = mgmt.template_associated_at else {
            return;
        };
        let marker = format!("{management}@{}", associated_at.to_rfc3339());
        if guard
            .get(account_id)
            .and_then(|d| d.template_checked.as_deref())
            == Some(marker.as_str())
        {
            return;
        }
        let entries: Vec<_> = if joined.is_some_and(|j| j >= associated_at) {
            mgmt.template.values().cloned().collect()
        } else {
            Vec::new()
        };
        (marker, entries)
    };

    let mut guard = state.write();
    let data = guard.get_or_create(account_id);
    if data.template_checked.as_deref() == Some(marker.as_str()) {
        return;
    }
    data.template_checked = Some(marker);
    for entry in entries {
        let Some(def) = catalog::quota(&entry.service_code, &entry.quota_code) else {
            continue;
        };
        let region = quota_region(&entry.aws_region, def);
        if applied_value(Some(data), &entry.aws_region, def) >= entry.desired_value {
            continue;
        }
        data.applied.insert(
            applied_key(
                &entry.aws_region,
                def.global,
                def.service_code,
                def.quota_code,
            ),
            entry.desired_value,
        );
        let caller =
            fakecloud_aws::arn::Arn::global_in(&entry.aws_region, "iam", &management, "root")
                .to_string();
        let id = new_request_id();
        data.requests.insert(
            id.clone(),
            QuotaRequest {
                id,
                region,
                service_code: def.service_code.to_string(),
                quota_code: def.quota_code.to_string(),
                desired_value: entry.desired_value,
                status: "APPROVED".to_string(),
                case_id: None,
                created: now,
                last_updated: now,
                requester: requester(account_id, &caller),
                quota_arn: quota_arn(&entry.aws_region, account_id, def),
            },
        );
    }
}

/// The [`QuotaProvider`] enforcing services resolve applied values through.
pub struct ServiceQuotasProvider {
    state: SharedServiceQuotasState,
    orgs: SharedOrganizationsState,
}

impl ServiceQuotasProvider {
    pub fn new(state: SharedServiceQuotasState, orgs: SharedOrganizationsState) -> Self {
        Self { state, orgs }
    }
}

impl QuotaProvider for ServiceQuotasProvider {
    fn applied_value(
        &self,
        account_id: &str,
        region: &str,
        service_code: &str,
        quota_code: &str,
    ) -> Option<f64> {
        let def = catalog::quota(service_code, quota_code)?;
        apply_template_if_new(&self.state, &self.orgs, account_id, Utc::now());
        let guard = self.state.read();
        Some(applied_value(guard.get(account_id), region, def))
    }
}
