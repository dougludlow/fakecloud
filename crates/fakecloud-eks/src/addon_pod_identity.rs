//! Pod identity associations an add-on manages.
//!
//! `CreateAddon` / `UpdateAddon` take `podIdentityAssociations`
//! (`{serviceAccount, roleArn}`) and EKS creates a real pod identity
//! association for each, owned by the add-on (`ownerArn`) in the add-on's
//! namespace. The add-on reports their ARNs, `DescribePodIdentityAssociation`
//! resolves them, an update keeps the association (and its ARN) of a service
//! account that stays, and deleting the add-on deletes them. Shared by the API
//! handlers and the CloudFormation provisioner.

use chrono::Utc;
use serde_json::Value;

use crate::state::{pod_identity_association_arn, EksState, PodIdentityAssociation};

/// The namespace EKS installs add-ons into unless `namespaceConfig` says
/// otherwise.
pub const DEFAULT_ADDON_NAMESPACE: &str = "kube-system";

/// Where an add-on's associations live and who owns them.
pub struct AddonOwner<'a> {
    pub region: &'a str,
    pub account_id: &'a str,
    pub cluster: &'a str,
    pub addon_arn: &'a str,
    pub namespace: &'a str,
}

/// Make the add-on's associations match `requested` (a
/// `podIdentityAssociations` list) and return their ARNs in request order.
/// `Err` carries the `InvalidParameterException` message; nothing changes on
/// error.
pub fn reconcile_addon_pod_identity_associations(
    state: &mut EksState,
    owner: &AddonOwner<'_>,
    requested: &Value,
) -> Result<Vec<String>, String> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for item in requested.as_array().into_iter().flatten() {
        let service_account = item
            .get("serviceAccount")
            .and_then(Value::as_str)
            .ok_or("podIdentityAssociations[].serviceAccount is required")?;
        let role_arn = item
            .get("roleArn")
            .and_then(Value::as_str)
            .ok_or("podIdentityAssociations[].roleArn is required")?;
        if wanted.iter().any(|(sa, _)| sa == service_account) {
            return Err(format!(
                "Service account {service_account} is specified more than once"
            ));
        }
        wanted.push((service_account.to_string(), role_arn.to_string()));
    }
    let map = state
        .pod_identity_associations
        .entry(owner.cluster.to_string())
        .or_default();
    // A service account another association (not this add-on's) already
    // covers in the namespace cannot be claimed.
    for (sa, _) in &wanted {
        if map.values().any(|a| {
            a.namespace == owner.namespace
                && &a.service_account == sa
                && a.owner_arn.as_deref() != Some(owner.addon_arn)
        }) {
            return Err(format!(
                "Association already exists for namespace {} and service account {sa}",
                owner.namespace
            ));
        }
    }
    // Drop this add-on's associations whose service account is gone.
    map.retain(|_, a| {
        a.owner_arn.as_deref() != Some(owner.addon_arn)
            || wanted.iter().any(|(sa, _)| *sa == a.service_account)
    });
    let now = Utc::now();
    let mut arns = Vec::with_capacity(wanted.len());
    for (sa, role) in wanted {
        if let Some(existing) = map
            .values_mut()
            .find(|a| a.owner_arn.as_deref() == Some(owner.addon_arn) && a.service_account == sa)
        {
            if existing.role_arn != role {
                existing.role_arn = role;
                existing.modified_at = now;
            }
            arns.push(existing.association_arn.clone());
            continue;
        }
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let suffix = &suffix[..17];
        let association_id = format!("a-{suffix}");
        let association_arn =
            pod_identity_association_arn(owner.region, owner.account_id, owner.cluster, suffix);
        map.insert(
            association_id.clone(),
            PodIdentityAssociation {
                cluster_name: owner.cluster.to_string(),
                namespace: owner.namespace.to_string(),
                service_account: sa,
                role_arn: role,
                association_arn: association_arn.clone(),
                association_id,
                created_at: now,
                modified_at: now,
                disable_session_tags: false,
                target_role_arn: None,
                external_id: None,
                tags: Default::default(),
                owner_arn: Some(owner.addon_arn.to_string()),
            },
        );
        arns.push(association_arn);
    }
    Ok(arns)
}

/// Delete every association the add-on `addon_arn` owns in `cluster`.
pub fn delete_addon_pod_identity_associations(
    state: &mut EksState,
    cluster: &str,
    addon_arn: &str,
) {
    if let Some(map) = state.pod_identity_associations.get_mut(cluster) {
        map.retain(|_, a| a.owner_arn.as_deref() != Some(addon_arn));
    }
}
