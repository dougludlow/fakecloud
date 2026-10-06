//! The `/_fakecloud/iam/create-admin` bootstrap.

use fakecloud_aws::arn::Arn;
use fakecloud_sdk::types;

/// Bootstrap an IAM admin user in a specific account. Creates the user,
/// access key, and an inline admin policy (`Allow */*`) in the target
/// account's IAM state. Returns the credentials so the caller can sign
/// requests as that user.
///
/// This solves the multi-account bootstrap problem: the `test*` root
/// bypass only targets the default account, so there's no way to create
/// credentials for a non-default account via the normal AWS API.
///
/// The account is standalone unless `organization_id` names an existing
/// organization, in which case it is enrolled into that organization's
/// root OU. That mirrors AWS: a freshly vended account belongs to no
/// organization until it is invited and accepts, or is created through
/// `CreateAccount`. Bootstrapping an admin must never silently pull the
/// account into an unrelated organization — that account then inherits
/// SCPs it never agreed to, can read the organization's metadata, and
/// becomes a stack-set auto-deployment target.
pub(crate) fn create_admin_in_account(
    iam: &fakecloud_iam::SharedIamState,
    organizations: &fakecloud_organizations::SharedOrganizationsState,
    account_id: &str,
    user_name: &str,
    organization_id: Option<&str>,
) -> Result<types::CreateAdminResponse, CreateAdminError> {
    if let Some(org_id) = organization_id {
        let mut guard = organizations.write();
        if !guard.contains_org(org_id) {
            return Err(CreateAdminError::UnknownOrganization(org_id.to_string()));
        }
        // An account belongs to at most one organization. Without this the
        // shortcut would enroll it into a second registry entry, and which
        // organization's SCP ceiling, DescribeOrganization view and
        // stack-set targeting applied would come down to org-id sort order.
        // `CreateOrganization` and `InviteAccountToOrganization` both reject
        // this; so does the shortcut.
        if let Some(other) = guard.claimed_by_other_org(account_id, org_id) {
            return Err(CreateAdminError::AccountInAnotherOrganization {
                account_id: account_id.to_string(),
                organization_id: other,
            });
        }
        let org = guard
            .org_by_id_mut(org_id)
            .expect("checked just above that the organization exists");
        org.enroll_account_if_missing(account_id);
    }

    let mut accounts = iam.write();
    let region = accounts.region().to_string();
    let state = accounts.get_or_create(account_id);

    let user_id = format!(
        "AIDA{}",
        &uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .to_uppercase()[..16]
    );
    let arn = Arn::global_in(&region, "iam", account_id, &format!("user/{user_name}")).to_string();
    let akid = format!(
        "FKIA{}",
        &uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .to_uppercase()[..20]
    );
    let secret = uuid::Uuid::new_v4().to_string();

    state.users.insert(
        user_name.to_string(),
        fakecloud_iam::IamUser {
            user_name: user_name.to_string(),
            user_id,
            arn: arn.clone(),
            path: "/".to_string(),
            created_at: chrono::Utc::now(),
            tags: Vec::new(),
            permissions_boundary: None,
        },
    );
    state.access_keys.insert(
        user_name.to_string(),
        vec![fakecloud_iam::IamAccessKey {
            access_key_id: akid.clone(),
            secret_access_key: secret.clone(),
            user_name: user_name.to_string(),
            status: "Active".to_string(),
            created_at: chrono::Utc::now(),
        }],
    );
    state.user_inline_policies.insert(
        user_name.to_string(),
        std::collections::BTreeMap::from([(
            "fakecloud-admin".to_string(),
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}"#.to_string(),
        )]),
    );

    Ok(types::CreateAdminResponse {
        access_key_id: akid,
        secret_access_key: secret,
        account_id: account_id.to_string(),
        arn,
    })
}

/// Why a `/_fakecloud/iam/create-admin` call could not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateAdminError {
    /// `organizationId` was supplied but no organization with that id
    /// exists. Enrolling into "whatever org happens to exist" is what
    /// the caller is explicitly avoiding by naming one, so this is an
    /// error rather than a silent fallback.
    UnknownOrganization(String),
    /// The account is already a member of a different organization, and
    /// an account can only ever be in one.
    AccountInAnotherOrganization {
        account_id: String,
        organization_id: String,
    },
}

impl CreateAdminError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::UnknownOrganization(id) => {
                format!("no organization with id {id} exists")
            }
            Self::AccountInAnotherOrganization {
                account_id,
                organization_id,
            } => format!(
                "account {account_id} is already a member of organization {organization_id}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[test]
    fn create_admin_in_default_account() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "123456789012", "admin", None)
            .expect("create admin");
        assert_eq!(resp.account_id, "123456789012");
        assert!(resp.access_key_id.starts_with("FKIA"));
        assert!(resp.arn.contains("123456789012"));
        assert!(resp.arn.contains("admin"));

        // Verify state was populated
        let accounts = iam.read();
        let state = accounts.get("123456789012").unwrap();
        assert!(state.users.contains_key("admin"));
        assert!(state.access_keys.contains_key("admin"));
        assert!(state.user_inline_policies.contains_key("admin"));
    }

    #[test]
    fn create_admin_on_a_china_server_uses_the_aws_cn_partition() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "cn-north-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", None)
            .expect("create admin");
        assert_eq!(resp.arn, "arn:aws-cn:iam::222222222222:user/admin");
        assert_eq!(
            iam.read().get("222222222222").unwrap().users["admin"].arn,
            resp.arn
        );
    }

    #[test]
    fn create_admin_in_new_account() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "999999999999", "bob", None)
            .expect("create admin");
        assert_eq!(resp.account_id, "999999999999");
        assert!(resp.arn.contains("999999999999"));

        // New account was created
        let accounts = iam.read();
        assert!(accounts.get("999999999999").is_some());
        let state = accounts.get("999999999999").unwrap();
        assert!(state.users.contains_key("bob"));

        // Default account untouched
        let default = accounts.get("123456789012").unwrap();
        assert!(default.users.is_empty());
    }

    #[test]
    fn create_admin_policy_allows_all() {
        use fakecloud_core::auth::{
            ConditionContext, IamAction, IamDecision, IamPolicyEvaluator, Principal, PrincipalType,
        };
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", None)
            .expect("create admin");

        let evaluator = fakecloud_iam::policy_evaluator::IamPolicyEvaluatorImpl::new(iam.clone());
        let principal = Principal {
            arn: resp.arn.clone(),
            user_id: "AIDATEST".to_string(),
            account_id: "222222222222".to_string(),
            principal_type: PrincipalType::User,
            source_identity: None,
            tags: None,
        };
        let action = IamAction {
            service: "s3",
            action: "ListBuckets",
            resource: "*".to_string(),
        };
        let decision =
            evaluator.evaluate(&principal, &action, &ConditionContext::default(), &[], None);
        assert_eq!(
            decision,
            IamDecision::Allow,
            "admin policy should Allow */*"
        );
    }

    /// Regression for #2543: bootstrapping an admin must not silently
    /// pull the account into an organization someone else created. An
    /// auto-joined account cannot become a management account of its
    /// own, which broke multi-organization setups.
    #[test]
    fn create_admin_does_not_join_existing_organization() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(
                fakecloud_organizations::OrganizationState::bootstrap("111111111111").into(),
            ));

        super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", None)
            .expect("create admin");

        let guard = orgs.read();
        let org = guard.sole().unwrap();
        assert!(
            !org.accounts.contains_key("222222222222"),
            "a standalone bootstrap must leave the account outside the org"
        );
        assert!(org.accounts.contains_key("111111111111"));
    }

    #[test]
    fn create_admin_with_organization_id_enrolls_account() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let org = fakecloud_organizations::OrganizationState::bootstrap("111111111111");
        let org_id = org.org_id.clone();
        let root_id = org.root_id.clone();
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(org.into()));

        super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&org_id))
            .expect("create admin");

        let guard = orgs.read();
        let member = guard
            .sole()
            .unwrap()
            .accounts
            .get("222222222222")
            .expect("account enrolled");
        assert_eq!(member.parent_id, root_id);
        assert_eq!(member.status, "ACTIVE");
    }

    /// An account belongs to at most one organization. The bootstrap
    /// shortcut must reject a second enrollment rather than putting the
    /// account in two registries at once, where which organization's SCP
    /// ceiling and stack-set targeting applied would be arbitrary.
    #[test]
    fn create_admin_cannot_enroll_an_account_into_a_second_organization() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let first = fakecloud_organizations::OrganizationState::bootstrap("111111111111");
        let second = fakecloud_organizations::OrganizationState::bootstrap("999999999999");
        let first_id = first.org_id.clone();
        let second_id = second.org_id.clone();
        let mut registry = fakecloud_organizations::OrganizationsRegistry::default();
        registry.insert(first);
        registry.insert(second);
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(registry));

        super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&first_id))
            .expect("first enrollment");

        let err =
            super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&second_id))
                .expect_err("a second organization must be rejected");
        assert_eq!(
            err,
            super::CreateAdminError::AccountInAnotherOrganization {
                account_id: "222222222222".to_string(),
                organization_id: first_id.clone(),
            }
        );

        let guard = orgs.read();
        assert!(guard
            .org_by_id(&first_id)
            .unwrap()
            .accounts
            .contains_key("222222222222"));
        assert!(!guard
            .org_by_id(&second_id)
            .unwrap()
            .accounts
            .contains_key("222222222222"));
    }

    /// Re-naming the organization the account is already in is a no-op
    /// rather than an error — bootstrapping admin credentials twice for
    /// the same member must keep working.
    #[test]
    fn create_admin_into_the_account_s_own_organization_is_idempotent() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let org = fakecloud_organizations::OrganizationState::bootstrap("111111111111");
        let org_id = org.org_id.clone();
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(org.into()));

        for _ in 0..2 {
            super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&org_id))
                .expect("repeat enrollment is a no-op");
        }
        assert_eq!(orgs.read().org_by_id(&org_id).unwrap().accounts.len(), 2);
    }

    #[test]
    fn create_admin_with_unknown_organization_id_errors() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(
                fakecloud_organizations::OrganizationState::bootstrap("111111111111").into(),
            ));

        let err = super::create_admin_in_account(
            &iam,
            &orgs,
            "222222222222",
            "admin",
            Some("o-doesnotexist"),
        )
        .expect_err("unknown org id must be rejected");
        assert_eq!(
            err,
            super::CreateAdminError::UnknownOrganization("o-doesnotexist".to_string())
        );
        // The IAM user is not created when the enrollment target is bogus.
        assert!(iam.read().get("222222222222").is_none());
    }

    #[test]
    fn create_admin_credentials_resolve() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "222222222222", "alice", None)
            .expect("create admin");

        // Verify the credential resolver can find this key
        let mut accounts = iam.write();
        let state = accounts.get_or_create("222222222222");
        let lookup = state.credential_secret(&resp.access_key_id);
        assert!(lookup.is_some());
        let lookup = lookup.unwrap();
        assert_eq!(lookup.account_id, "222222222222");
        assert_eq!(lookup.secret_access_key, resp.secret_access_key);
    }
}
