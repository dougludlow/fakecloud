//! Identity Store (`identitystore`) awsJson1.1 dispatch + operation handlers.
//!
//! The full 22-operation control plane: users, groups, and the memberships
//! linking them, plus the attribute-lookup helpers
//! (`GetUserId`/`GetGroupId`/`GetGroupMembershipId`), `IsMemberInGroups`, and
//! the identity-store resource itself (`DescribeIdentityStore`,
//! `ListIdentityStores`, `UpdateIdentityStore`).
//! State is account-partitioned and persisted. Nested SCIM attribute bags are
//! stored as the raw request `Value` so they round-trip verbatim.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use fakecloud_aws::arn::{arn_resource, Arn};
use fakecloud_core::service::{AwsRequest, AwsResponse, AwsService, AwsServiceError};
use fakecloud_persistence::SnapshotStore;

use crate::persistence::save_snapshot;
use crate::state::{
    initial_revision, SharedIdentityStoreState, StoredGroup, StoredMembership, StoredUser,
};

/// Every operation name in the Identity Store Smithy model.
pub const IDENTITYSTORE_ACTIONS: &[&str] = &[
    "CreateGroup",
    "CreateGroupMembership",
    "CreateUser",
    "DeleteGroup",
    "DeleteGroupMembership",
    "DeleteUser",
    "DescribeGroup",
    "DescribeGroupMembership",
    "DescribeIdentityStore",
    "DescribeUser",
    "GetGroupId",
    "GetGroupMembershipId",
    "GetUserId",
    "IsMemberInGroups",
    "ListGroupMemberships",
    "ListGroupMembershipsForMember",
    "ListGroups",
    "ListIdentityStores",
    "ListUsers",
    "UpdateGroup",
    "UpdateIdentityStore",
    "UpdateUser",
];

/// Free-form user profile attributes, all `SensitiveStringType` (@length
/// 1..=1024) in the Smithy model.
const SENSITIVE_USER_FIELDS: &[&str] = &[
    "DisplayName",
    "NickName",
    "ProfileUrl",
    "UserType",
    "Title",
    "PreferredLanguage",
    "Locale",
    "Timezone",
    "Website",
    "Birthdate",
];

/// Resolves the identity-store ids an account owns through its IAM Identity
/// Center instances (the SSO Admin control plane is the source of truth for
/// which stores were provisioned). Wired by the server so this crate does not
/// depend on the SSO Admin crate.
pub type InstanceStoreLookup = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

pub struct IdentityStoreService {
    state: SharedIdentityStoreState,
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
    snapshot_lock: Arc<AsyncMutex<()>>,
    instance_stores: Option<InstanceStoreLookup>,
}

impl IdentityStoreService {
    pub fn new(state: SharedIdentityStoreState) -> Self {
        Self {
            state,
            snapshot_store: None,
            snapshot_lock: Arc::new(AsyncMutex::new(())),
            instance_stores: None,
        }
    }

    /// Attach the lookup for identity stores provisioned by IAM Identity
    /// Center instances, so `ListIdentityStores`/`DescribeIdentityStore`/
    /// `UpdateIdentityStore` see them before any directory write.
    pub fn with_instance_store_lookup(mut self, lookup: InstanceStoreLookup) -> Self {
        self.instance_stores = Some(lookup);
        self
    }

    /// Every identity-store id `account` owns: the stores of its Identity
    /// Center instances plus any directory created by a write, sorted and
    /// de-duplicated.
    fn known_stores(&self, account: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .instance_stores
            .as_ref()
            .map(|f| f(account))
            .unwrap_or_default();
        if let Some(acct) = self.state.read().get(account) {
            ids.extend(acct.stores.keys().cloned());
        }
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// Persist hook for callers that change this service's state from outside
    /// (the reset endpoints): writes the current snapshot. `None` in memory
    /// mode.
    pub fn snapshot_hook(&self) -> Option<fakecloud_persistence::SnapshotHook> {
        let store = self.snapshot_store.clone()?;
        let state = self.state.clone();
        let lock = self.snapshot_lock.clone();
        Some(Arc::new(move || {
            let state = state.clone();
            let store = store.clone();
            let lock = lock.clone();
            Box::pin(async move {
                save_snapshot(&state, Some(store), &lock).await;
            })
        }))
    }

    async fn save(&self) {
        save_snapshot(
            &self.state,
            self.snapshot_store.clone(),
            &self.snapshot_lock,
        )
        .await;
    }
}

#[async_trait]
impl AwsService for IdentityStoreService {
    fn service_name(&self) -> &str {
        "identitystore"
    }

    async fn handle(&self, request: AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let mutates = is_mutating(request.action.as_str());
        let result = dispatch(self, &request);
        if mutates && matches!(result.as_ref(), Ok(resp) if resp.status.is_success()) {
            self.save().await;
        }
        result
    }

    fn supported_actions(&self) -> &[&str] {
        IDENTITYSTORE_ACTIONS
    }
}

fn is_mutating(action: &str) -> bool {
    action.starts_with("Create") || action.starts_with("Delete") || action.starts_with("Update")
}

fn dispatch(s: &IdentityStoreService, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
    match req.action.as_str() {
        "CreateUser" => s.create_user(req),
        "DescribeUser" => s.describe_user(req),
        "UpdateUser" => s.update_user(req),
        "DeleteUser" => s.delete_user(req),
        "GetUserId" => s.get_user_id(req),
        "ListUsers" => s.list_users(req),
        "CreateGroup" => s.create_group(req),
        "DescribeGroup" => s.describe_group(req),
        "UpdateGroup" => s.update_group(req),
        "DeleteGroup" => s.delete_group(req),
        "GetGroupId" => s.get_group_id(req),
        "ListGroups" => s.list_groups(req),
        "CreateGroupMembership" => s.create_group_membership(req),
        "DescribeGroupMembership" => s.describe_group_membership(req),
        "DeleteGroupMembership" => s.delete_group_membership(req),
        "GetGroupMembershipId" => s.get_group_membership_id(req),
        "ListGroupMemberships" => s.list_group_memberships(req),
        "ListGroupMembershipsForMember" => s.list_group_memberships_for_member(req),
        "IsMemberInGroups" => s.is_member_in_groups(req),
        "DescribeIdentityStore" => s.describe_identity_store(req),
        "ListIdentityStores" => s.list_identity_stores(req),
        "UpdateIdentityStore" => s.update_identity_store(req),
        _ => Err(AwsServiceError::action_not_implemented(
            s.service_name(),
            &req.action,
        )),
    }
}

// ===== helpers =====

fn ok(v: Value) -> Result<AwsResponse, AwsServiceError> {
    Ok(AwsResponse::json_value(StatusCode::OK, v))
}

fn parse(req: &AwsRequest) -> Result<Value, AwsServiceError> {
    if req.body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&req.body)
        .map_err(|e| validation(&format!("Request body is malformed: {e}")))
}

fn validation(msg: &str) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", msg)
}

fn not_found(msg: &str) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::NOT_FOUND, "ResourceNotFoundException", msg)
}

fn conflict(msg: &str) -> AwsServiceError {
    conflict_with_reason(msg, "UNIQUENESS_CONSTRAINT_VIOLATION")
}

/// `ConflictException` carrying its modeled `Reason`
/// (`UNIQUENESS_CONSTRAINT_VIOLATION` | `CONCURRENT_MODIFICATION`).
fn conflict_with_reason(msg: &str, reason: &str) -> AwsServiceError {
    // `ConflictException` is modeled with `@httpError(409)`; the SDKs and
    // Terraform's retry/error handling key on the 409 status, so returning 400
    // here would misclassify a duplicate-resource conflict as a plain
    // client-validation error.
    AwsServiceError::aws_error_with_fields(
        StatusCode::CONFLICT,
        "ConflictException",
        msg,
        vec![("Reason".to_string(), reason.to_string())],
    )
}

/// Read and validate the optional `Revision` (`ResourceRevision`: @length
/// 1..=64, @pattern `^[0-9]+$`) of an update/delete request. Validated
/// up front, whether or not the target resource exists.
fn req_revision(b: &Value) -> Result<Option<&str>, AwsServiceError> {
    let Some(v) = b.get("Revision") else {
        return Ok(None);
    };
    v.as_str()
        .filter(|r| (1..=64).contains(&r.len()) && r.bytes().all(|c| c.is_ascii_digit()))
        .map(Some)
        .ok_or_else(|| validation("Revision must be a string of 1 to 64 digits matching ^[0-9]+$."))
}

/// When an expected revision was supplied, require it to equal the resource's
/// `current` revision; a mismatch is `ConflictException`
/// (`CONCURRENT_MODIFICATION`).
fn check_revision(expected: Option<&str>, current: u64, kind: &str) -> Result<(), AwsServiceError> {
    let Some(rev) = expected else {
        return Ok(());
    };
    // Revisions are opaque tokens: compare them verbatim.
    if rev != current.to_string() {
        return Err(conflict_with_reason(
            &format!(
                "The {kind} was modified concurrently: expected revision {rev} but the current revision is {current}."
            ),
            "CONCURRENT_MODIFICATION",
        ));
    }
    Ok(())
}

/// The ARN of a user/group/membership. These are global, account-less ARNs:
/// `arn:<partition>:identitystore:::<kind>/<id>`.
fn resource_arn(region: &str, kind: &str, id: &str) -> String {
    Arn::global_in(region, "identitystore", "", &format!("{kind}/{id}")).to_string()
}

/// The ARN of an identity store:
/// `arn:<partition>:identitystore::<account>:identitystore/<id>`.
fn identity_store_arn(region: &str, account: &str, store: &str) -> String {
    Arn::global_in(
        region,
        "identitystore",
        account,
        &format!("identitystore/{store}"),
    )
    .to_string()
}

/// Mint a user/group/membership id. Stores with a canonical `d-<10 hex>` id
/// get AWS's `<10 hex>-<UUID>` form (the store id's hex prefixed onto a random
/// UUID); any other store id gets a bare UUID, the legacy-store format.
fn new_resource_id(store: &str) -> String {
    let uuid = Uuid::new_v4();
    match store.strip_prefix("d-") {
        Some(hex)
            if hex.len() == 10
                && hex
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) =>
        {
            format!("{hex}-{uuid}")
        }
        _ => uuid.to_string(),
    }
}

fn req_str<'a>(b: &'a Value, f: &str) -> Result<&'a str, AwsServiceError> {
    b.get(f)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| validation(&format!("{f} must be specified.")))
}

/// Character-length bound check for a present string field, matching the
/// Smithy `@length` constraint. Absent fields are the caller's concern.
fn check_len(b: &Value, field: &str, min: usize, max: usize) -> Result<(), AwsServiceError> {
    if let Some(s) = b.get(field).and_then(Value::as_str) {
        let n = s.chars().count();
        if n < min || n > max {
            return Err(validation(&format!(
                "{field} must have length between {min} and {max}, inclusive."
            )));
        }
    }
    Ok(())
}

/// Read the required `IdentityStoreId`. The model accepts either the bare id
/// (`d-1234567890`) or the store's ARN
/// (`arn:aws:identitystore::111122223333:identitystore/d-1234567890`); both
/// resolve to the bare id that keys the directory. Stores are per-account, so
/// an ARN naming another account's store never resolves to the caller's
/// directory of the same id: it is `ResourceNotFoundException`.
fn store_id(b: &Value, account: &str) -> Result<String, AwsServiceError> {
    let s = req_str(b, "IdentityStoreId")?;
    // IdentityStoreId @length 1..=93.
    if s.chars().count() > 93 {
        return Err(validation(
            "IdentityStoreId must have length between 1 and 93, inclusive.",
        ));
    }
    if s.starts_with("arn:") {
        let (arn_account, id) = arn_resource(s, "identitystore")
            .and_then(|r| r.strip_prefix(':'))
            .and_then(|r| r.split_once(":identitystore/"))
            .filter(|(acct, id)| !acct.is_empty() && !id.is_empty())
            .ok_or_else(|| validation("IdentityStoreId is not a valid identity store ARN."))?;
        if arn_account != account {
            return Err(store_not_found(id));
        }
        return Ok(id.to_string());
    }
    Ok(s.to_string())
}

/// `ResourceNotFoundException` for an identity store (`IDENTITY_STORE`).
fn store_not_found(sid: &str) -> AwsServiceError {
    AwsServiceError::aws_error_with_fields(
        StatusCode::NOT_FOUND,
        "ResourceNotFoundException",
        format!("Identity store {sid} not found."),
        vec![
            ("ResourceType".to_string(), "IDENTITY_STORE".to_string()),
            ("ResourceId".to_string(), sid.to_string()),
        ],
    )
}

/// Resolve a `ResourceId` value (`@length 1..=100`) to the bare id. The model
/// accepts the bare id or the resource's account-less ARN
/// (`arn:aws:identitystore:::<kind>/<id>`); an ARN must name a resource of the
/// `kind` the field expects (`user`, `group` or `membership`).
fn parse_resource_id(s: &str, field: &str, kind: &str) -> Result<String, AwsServiceError> {
    let n = s.chars().count();
    if n == 0 || n > 100 {
        return Err(validation(&format!(
            "{field} must have length between 1 and 100, inclusive."
        )));
    }
    if !s.starts_with("arn:") {
        return Ok(s.to_string());
    }
    let (arn_kind, id) = arn_resource(s, "identitystore")
        .and_then(|r| r.strip_prefix("::"))
        .and_then(|r| r.split_once('/'))
        .filter(|(_, id)| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| validation(&format!("{field} is not a valid identity store ARN.")))?;
    if arn_kind != kind {
        return Err(validation(&format!(
            "{field} must identify a {kind}, but the ARN names a {arn_kind}."
        )));
    }
    Ok(id.to_string())
}

/// Read a required `ResourceId` field naming a resource of `kind`.
fn resource_id(b: &Value, field: &str, kind: &str) -> Result<String, AwsServiceError> {
    parse_resource_id(req_str(b, field)?, field, kind)
}

/// Validate the `MaxResults` (@range 1..=100) and `NextToken` (@length
/// 1..=65535) pagination inputs shared by the list operations.
fn check_pagination(b: &Value) -> Result<(), AwsServiceError> {
    if let Some(v) = b.get("MaxResults") {
        let n = v.as_i64().unwrap_or(-1);
        if !(1..=100).contains(&n) {
            return Err(validation(
                "MaxResults must be between 1 and 100, inclusive.",
            ));
        }
    }
    check_len(b, "NextToken", 1, 65535)
}

fn epoch(dt: &DateTime<Utc>) -> Value {
    json!(dt.timestamp())
}

/// `MemberId` is a union; only the `UserId` member is modeled today. Its value
/// is a `ResourceId` (bare user id or user ARN).
fn member_user_id(b: &Value) -> Result<String, AwsServiceError> {
    let id = b
        .get("MemberId")
        .and_then(|m| m.get("UserId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| validation("MemberId.UserId must be specified."))?;
    parse_resource_id(id, "MemberId.UserId", "user")
}

/// Validate pagination inputs, then window an ordered slice of result rows.
fn paginate(rows: Vec<Value>, b: &Value) -> Result<(Vec<Value>, Option<String>), AwsServiceError> {
    check_pagination(b)?;
    let start = b
        .get("NextToken")
        .and_then(Value::as_str)
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0);
    let max = b
        .get("MaxResults")
        .and_then(Value::as_u64)
        .map(|m| m.clamp(1, 100) as usize)
        .unwrap_or(100);
    let end = (start + max).min(rows.len());
    let page = rows.get(start..end).unwrap_or(&[]).to_vec();
    let next = if end < rows.len() {
        Some(end.to_string())
    } else {
        None
    };
    Ok((page, next))
}

/// Resolve `seg` to a bag key: reuse an existing key that matches
/// case-insensitively, otherwise fall back to a PascalCase form of `seg`.
///
/// SCIM `AttributeOperations` (as emitted by the Terraform provider and other
/// SCIM clients) address attributes in camelCase (`name.givenName`,
/// `displayName`, `phoneNumbers`), but the attribute bag is stored with the
/// PascalCase member names the awsJson `CreateUser`/`CreateGroup` request
/// carried (`Name.GivenName`, `DisplayName`, `PhoneNumbers`). Without this
/// mapping an update would write to a shadow lowercase key and the describe
/// would keep returning the stale PascalCase value.
fn canonical_key(bag: &Map<String, Value>, seg: &str) -> String {
    if let Some(existing) = bag.keys().find(|k| k.eq_ignore_ascii_case(seg)) {
        return existing.clone();
    }
    let mut chars = seg.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => seg.to_string(),
    }
}

/// Set a (possibly dotted) attribute path on the bag; `None` removes it.
fn set_attribute(bag: &mut Map<String, Value>, path: &str, value: Option<Value>) {
    let mut parts = path.split('.').peekable();
    let head_raw = match parts.next() {
        Some(h) => h,
        None => return,
    };
    let head = canonical_key(bag, head_raw);
    if parts.peek().is_none() {
        match value {
            Some(v) => {
                bag.insert(head, v);
            }
            None => {
                bag.remove(&head);
            }
        }
        return;
    }
    let rest: String = parts.collect::<Vec<_>>().join(".");
    let child = bag.entry(head).or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(m) = child {
        set_attribute(m, &rest, value);
    }
}

/// Recursively PascalCase the first letter of every object key. SCIM update
/// clients (the Terraform provider in particular) hand-build the
/// `AttributeValue` of a list operation with camelCase element keys
/// (`{value, type, primary}`, `{streetAddress, postalCode, country}`), but the
/// awsJson model — and therefore every SDK deserializing our `DescribeUser`
/// response — uses PascalCase member names (`Value`, `Type`, `StreetAddress`).
/// Canonicalizing on the way in keeps the stored bag model-conformant so the
/// SDK reads the sub-fields back. Scalars are returned unchanged; already
/// PascalCase keys are idempotent.
fn canonicalize_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, val)| {
                    let mut chars = k.chars();
                    let key = match chars.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                        None => k,
                    };
                    (key, canonicalize_keys(val))
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(canonicalize_keys).collect()),
        other => other,
    }
}

fn apply_operations(attributes: &mut Value, ops: &Value) {
    let Value::Object(bag) = attributes else {
        return;
    };
    let Some(list) = ops.as_array() else {
        return;
    };
    for op in list {
        let Some(path) = op.get("AttributePath").and_then(Value::as_str) else {
            continue;
        };
        set_attribute(
            bag,
            path,
            op.get("AttributeValue").cloned().map(canonicalize_keys),
        );
    }
}

fn build_user(u: &StoredUser, store: &str, region: &str) -> Value {
    let mut m = u.attributes.as_object().cloned().unwrap_or_default();
    m.insert("IdentityStoreId".into(), json!(store));
    m.insert("UserId".into(), json!(u.user_id));
    m.insert(
        "UserArn".into(),
        json!(resource_arn(region, "user", &u.user_id)),
    );
    m.insert("Revision".into(), json!(u.revision.to_string()));
    m.entry("UserStatus").or_insert(json!("ENABLED"));
    m.insert("CreatedAt".into(), epoch(&u.created_at));
    m.insert("UpdatedAt".into(), epoch(&u.updated_at));
    Value::Object(m)
}

fn build_group(g: &StoredGroup, store: &str, region: &str) -> Value {
    let mut m = g.attributes.as_object().cloned().unwrap_or_default();
    m.insert("IdentityStoreId".into(), json!(store));
    m.insert("GroupId".into(), json!(g.group_id));
    m.insert(
        "GroupArn".into(),
        json!(resource_arn(region, "group", &g.group_id)),
    );
    m.insert("Revision".into(), json!(g.revision.to_string()));
    m.insert("CreatedAt".into(), epoch(&g.created_at));
    m.insert("UpdatedAt".into(), epoch(&g.updated_at));
    Value::Object(m)
}

fn build_membership(m: &StoredMembership, store: &str, region: &str) -> Value {
    json!({
        "IdentityStoreId": store,
        "MembershipId": m.membership_id,
        "MembershipArn": resource_arn(region, "membership", &m.membership_id),
        "GroupId": m.group_id,
        "MemberId": { "UserId": m.member_user_id },
        "CreatedAt": epoch(&m.created_at),
        "UpdatedAt": epoch(&m.updated_at),
    })
}

/// Equality filters (`Filters: [{AttributePath, AttributeValue}]`) are a
/// deprecated-but-still-emitted request shape; apply them as top-level string
/// equality so older SDKs / Terraform data sources behave.
fn matches_filters(bag: &Value, filters: Option<&Value>) -> bool {
    let Some(arr) = filters.and_then(Value::as_array) else {
        return true;
    };
    arr.iter().all(|f| {
        let path = f.get("AttributePath").and_then(Value::as_str);
        let want = f.get("AttributeValue").and_then(Value::as_str);
        match (path, want) {
            (Some(p), Some(w)) => bag.get(p).and_then(Value::as_str) == Some(w),
            _ => true,
        }
    })
}

impl IdentityStoreService {
    // ---- users ----

    fn create_user(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        // UserName @length 1..=128; the free-form profile attributes are all
        // `SensitiveStringType` @length 1..=1024.
        check_len(&b, "UserName", 1, 128)?;
        for field in SENSITIVE_USER_FIELDS {
            check_len(&b, field, 1, 1024)?;
        }
        let user_name = b
            .get("UserName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut guard = self.state.write();
        let acct = guard.get_or_create(&req.account_id);
        let dir = acct.stores.entry(sid.clone()).or_default();
        if let Some(name) = &user_name {
            if dir
                .users
                .values()
                .any(|u| u.user_name.as_deref() == Some(name))
            {
                return Err(conflict(&format!(
                    "User with the specified UserName `{name}` already exists."
                )));
            }
        }
        let user_id = new_resource_id(&sid);
        let now = Utc::now();
        // Strip control/routing members from the persisted attribute bag.
        let mut attributes = b.clone();
        if let Value::Object(m) = &mut attributes {
            m.remove("IdentityStoreId");
        }
        let revision = initial_revision();
        dir.users.insert(
            user_id.clone(),
            StoredUser {
                user_id: user_id.clone(),
                user_name,
                attributes,
                revision,
                created_at: now,
                updated_at: now,
            },
        );
        ok(json!({
            "IdentityStoreId": sid,
            "UserId": user_id,
            "UserArn": resource_arn(&req.region, "user", &user_id),
            "Revision": revision.to_string(),
        }))
    }

    fn describe_user(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let user_id = resource_id(&b, "UserId", "user")?;
        let guard = self.state.read();
        let dir = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .ok_or_else(|| not_found("USER not found."))?;
        let u = dir
            .users
            .get(&user_id)
            .ok_or_else(|| not_found("USER not found."))?;
        ok(build_user(u, &sid, &req.region))
    }

    fn update_user(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let user_id = resource_id(&b, "UserId", "user")?;
        let revision = req_revision(&b)?;
        let ops = b
            .get("Operations")
            .cloned()
            .ok_or_else(|| validation("Operations must be specified."))?;
        let mut guard = self.state.write();
        let dir = guard
            .get_mut(&req.account_id)
            .and_then(|a| a.stores.get_mut(&sid))
            .ok_or_else(|| not_found("USER not found."))?;
        let u = dir
            .users
            .get_mut(&user_id)
            .ok_or_else(|| not_found("USER not found."))?;
        check_revision(revision, u.revision, "user")?;
        apply_operations(&mut u.attributes, &ops);
        u.user_name = u
            .attributes
            .get("UserName")
            .and_then(Value::as_str)
            .map(str::to_string);
        u.revision += 1;
        u.updated_at = Utc::now();
        ok(json!({
            "IdentityStoreId": sid,
            "UserId": user_id,
            "UserArn": resource_arn(&req.region, "user", &user_id),
            "Revision": u.revision.to_string(),
        }))
    }

    fn delete_user(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let user_id = resource_id(&b, "UserId", "user")?;
        let revision = req_revision(&b)?;
        let mut guard = self.state.write();
        if let Some(dir) = guard
            .get_mut(&req.account_id)
            .and_then(|a| a.stores.get_mut(&sid))
        {
            if let Some(u) = dir.users.get(&user_id) {
                check_revision(revision, u.revision, "user")?;
            }
            dir.users.remove(&user_id);
            dir.memberships.retain(|_, m| m.member_user_id != user_id);
        }
        ok(json!({}))
    }

    fn get_user_id(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let (path, want) = alternate_identifier(&b)?;
        let guard = self.state.read();
        let dir = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .ok_or_else(|| not_found("USER not found."))?;
        let found = dir.users.values().find(|u| {
            if path.eq_ignore_ascii_case("UserName") {
                u.user_name.as_deref() == Some(want.as_str())
            } else {
                attribute_matches(&u.attributes, &path, &want)
            }
        });
        match found {
            Some(u) => ok(json!({
                "IdentityStoreId": sid,
                "UserId": u.user_id,
                "UserArn": resource_arn(&req.region, "user", &u.user_id),
            })),
            None => Err(not_found("USER not found.")),
        }
    }

    fn list_users(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let guard = self.state.read();
        let rows: Vec<Value> = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .map(|dir| {
                dir.users
                    .values()
                    .filter(|u| matches_filters(&u.attributes, b.get("Filters")))
                    .map(|u| build_user(u, &sid, &req.region))
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(rows, &b)?;
        let mut out = json!({ "Users": page });
        if let Some(t) = next {
            out["NextToken"] = json!(t);
        }
        ok(out)
    }

    // ---- groups ----

    fn create_group(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        // DisplayName is `GroupDisplayName` @length 1..=1024; Description is
        // `SensitiveStringType` @length 1..=1024.
        check_len(&b, "DisplayName", 1, 1024)?;
        check_len(&b, "Description", 1, 1024)?;
        let display_name = b
            .get("DisplayName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut guard = self.state.write();
        let acct = guard.get_or_create(&req.account_id);
        let dir = acct.stores.entry(sid.clone()).or_default();
        if let Some(name) = &display_name {
            if dir
                .groups
                .values()
                .any(|g| g.display_name.as_deref() == Some(name))
            {
                return Err(conflict(&format!(
                    "Group with the specified DisplayName `{name}` already exists."
                )));
            }
        }
        let group_id = new_resource_id(&sid);
        let now = Utc::now();
        let mut attributes = b.clone();
        if let Value::Object(m) = &mut attributes {
            m.remove("IdentityStoreId");
        }
        let revision = initial_revision();
        dir.groups.insert(
            group_id.clone(),
            StoredGroup {
                group_id: group_id.clone(),
                display_name,
                attributes,
                revision,
                created_at: now,
                updated_at: now,
            },
        );
        ok(json!({
            "GroupId": group_id,
            "IdentityStoreId": sid,
            "GroupArn": resource_arn(&req.region, "group", &group_id),
            "Revision": revision.to_string(),
        }))
    }

    fn describe_group(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let group_id = resource_id(&b, "GroupId", "group")?;
        let guard = self.state.read();
        let g = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .and_then(|d| d.groups.get(&group_id))
            .ok_or_else(|| not_found("GROUP not found."))?;
        ok(build_group(g, &sid, &req.region))
    }

    fn update_group(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let group_id = resource_id(&b, "GroupId", "group")?;
        let revision = req_revision(&b)?;
        let ops = b
            .get("Operations")
            .cloned()
            .ok_or_else(|| validation("Operations must be specified."))?;
        let mut guard = self.state.write();
        let g = guard
            .get_mut(&req.account_id)
            .and_then(|a| a.stores.get_mut(&sid))
            .and_then(|d| d.groups.get_mut(&group_id))
            .ok_or_else(|| not_found("GROUP not found."))?;
        check_revision(revision, g.revision, "group")?;
        apply_operations(&mut g.attributes, &ops);
        g.display_name = g
            .attributes
            .get("DisplayName")
            .and_then(Value::as_str)
            .map(str::to_string);
        g.revision += 1;
        g.updated_at = Utc::now();
        ok(json!({
            "GroupId": group_id,
            "IdentityStoreId": sid,
            "GroupArn": resource_arn(&req.region, "group", &group_id),
            "Revision": g.revision.to_string(),
        }))
    }

    fn delete_group(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let group_id = resource_id(&b, "GroupId", "group")?;
        let revision = req_revision(&b)?;
        let mut guard = self.state.write();
        if let Some(dir) = guard
            .get_mut(&req.account_id)
            .and_then(|a| a.stores.get_mut(&sid))
        {
            if let Some(g) = dir.groups.get(&group_id) {
                check_revision(revision, g.revision, "group")?;
            }
            dir.groups.remove(&group_id);
            dir.memberships.retain(|_, m| m.group_id != group_id);
        }
        ok(json!({}))
    }

    fn get_group_id(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let (path, want) = alternate_identifier(&b)?;
        let guard = self.state.read();
        let dir = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .ok_or_else(|| not_found("GROUP not found."))?;
        let found = dir.groups.values().find(|g| {
            if path.eq_ignore_ascii_case("DisplayName") {
                g.display_name.as_deref() == Some(want.as_str())
            } else {
                attribute_matches(&g.attributes, &path, &want)
            }
        });
        match found {
            Some(g) => ok(json!({
                "GroupId": g.group_id,
                "IdentityStoreId": sid,
                "GroupArn": resource_arn(&req.region, "group", &g.group_id),
            })),
            None => Err(not_found("GROUP not found.")),
        }
    }

    fn list_groups(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let guard = self.state.read();
        let rows: Vec<Value> = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .map(|dir| {
                dir.groups
                    .values()
                    .filter(|g| matches_filters(&g.attributes, b.get("Filters")))
                    .map(|g| build_group(g, &sid, &req.region))
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(rows, &b)?;
        let mut out = json!({ "Groups": page });
        if let Some(t) = next {
            out["NextToken"] = json!(t);
        }
        ok(out)
    }

    // ---- memberships ----

    fn create_group_membership(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let group_id = resource_id(&b, "GroupId", "group")?;
        let member = member_user_id(&b)?;
        let mut guard = self.state.write();
        let acct = guard.get_or_create(&req.account_id);
        let dir = acct.stores.entry(sid.clone()).or_default();
        if !dir.groups.contains_key(&group_id) {
            return Err(not_found("GROUP not found."));
        }
        if !dir.users.contains_key(&member) {
            return Err(not_found("USER not found."));
        }
        if let Some(existing) = dir
            .memberships
            .values()
            .find(|m| m.group_id == group_id && m.member_user_id == member)
        {
            return Err(conflict(&format!(
                "Membership `{}` already exists.",
                existing.membership_id
            )));
        }
        let membership_id = new_resource_id(&sid);
        let now = Utc::now();
        dir.memberships.insert(
            membership_id.clone(),
            StoredMembership {
                membership_id: membership_id.clone(),
                group_id,
                member_user_id: member,
                created_at: now,
                updated_at: now,
            },
        );
        ok(json!({
            "MembershipId": membership_id,
            "IdentityStoreId": sid,
            "MembershipArn": resource_arn(&req.region, "membership", &membership_id),
        }))
    }

    fn describe_group_membership(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let membership_id = resource_id(&b, "MembershipId", "membership")?;
        let guard = self.state.read();
        let m = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .and_then(|d| d.memberships.get(&membership_id))
            .ok_or_else(|| not_found("MEMBERSHIP not found."))?;
        ok(build_membership(m, &sid, &req.region))
    }

    fn delete_group_membership(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let membership_id = resource_id(&b, "MembershipId", "membership")?;
        let mut guard = self.state.write();
        if let Some(dir) = guard
            .get_mut(&req.account_id)
            .and_then(|a| a.stores.get_mut(&sid))
        {
            dir.memberships.remove(&membership_id);
        }
        ok(json!({}))
    }

    fn get_group_membership_id(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let group_id = resource_id(&b, "GroupId", "group")?;
        let member = member_user_id(&b)?;
        let guard = self.state.read();
        let m = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .and_then(|d| {
                d.memberships
                    .values()
                    .find(|m| m.group_id == group_id && m.member_user_id == member)
            })
            .ok_or_else(|| not_found("MEMBERSHIP not found."))?;
        ok(json!({
            "MembershipId": m.membership_id,
            "IdentityStoreId": sid,
            "MembershipArn": resource_arn(&req.region, "membership", &m.membership_id),
        }))
    }

    fn list_group_memberships(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let group_id = resource_id(&b, "GroupId", "group")?;
        let guard = self.state.read();
        let rows: Vec<Value> = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .map(|dir| {
                dir.memberships
                    .values()
                    .filter(|m| m.group_id == group_id)
                    .map(|m| build_membership(m, &sid, &req.region))
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(rows, &b)?;
        let mut out = json!({ "GroupMemberships": page });
        if let Some(t) = next {
            out["NextToken"] = json!(t);
        }
        ok(out)
    }

    fn list_group_memberships_for_member(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let member = member_user_id(&b)?;
        let guard = self.state.read();
        let rows: Vec<Value> = guard
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .map(|dir| {
                dir.memberships
                    .values()
                    .filter(|m| m.member_user_id == member)
                    .map(|m| build_membership(m, &sid, &req.region))
                    .collect()
            })
            .unwrap_or_default();
        let (page, next) = paginate(rows, &b)?;
        let mut out = json!({ "GroupMemberships": page });
        if let Some(t) = next {
            out["NextToken"] = json!(t);
        }
        ok(out)
    }

    fn is_member_in_groups(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = store_id(&b, &req.account_id)?;
        let member = member_user_id(&b)?;
        let group_ids: Vec<String> = b
            .get("GroupIds")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .ok_or_else(|| validation("GroupIds must be specified."))?;
        // Each element is a `GroupId` (`ResourceId`: bare id or group ARN).
        let group_ids = group_ids
            .iter()
            .map(|g| parse_resource_id(g, "GroupIds", "group"))
            .collect::<Result<Vec<_>, _>>()?;
        let guard = self.state.read();
        let dir = guard.get(&req.account_id).and_then(|a| a.stores.get(&sid));
        let results: Vec<Value> = group_ids
            .iter()
            .map(|gid| {
                let exists = dir
                    .map(|d| {
                        d.memberships
                            .values()
                            .any(|m| &m.group_id == gid && m.member_user_id == member)
                    })
                    .unwrap_or(false);
                json!({ "GroupId": gid, "MembershipExists": exists })
            })
            .collect();
        ok(json!({ "Results": results }))
    }
}

impl IdentityStoreService {
    // ---- identity stores ----

    /// Resolve the request's `IdentityStoreId` (id or ARN) to a store the
    /// account owns, or `ResourceNotFoundException` (`IDENTITY_STORE`).
    fn existing_store(&self, b: &Value, account: &str) -> Result<String, AwsServiceError> {
        let sid = store_id(b, account)?;
        if self.known_stores(account).contains(&sid) {
            Ok(sid)
        } else {
            Err(store_not_found(&sid))
        }
    }

    fn describe_identity_store(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = self.existing_store(&b, &req.account_id)?;
        let mut out = json!({
            "IdentityStoreId": sid,
            "IdentityStoreArn": identity_store_arn(&req.region, &req.account_id, &sid),
        });
        if let Some(nc) = self
            .state
            .read()
            .get(&req.account_id)
            .and_then(|a| a.stores.get(&sid))
            .and_then(|d| d.network_configuration.clone())
        {
            out["NetworkConfiguration"] = nc;
        }
        ok(out)
    }

    fn list_identity_stores(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let rows: Vec<Value> = self
            .known_stores(&req.account_id)
            .into_iter()
            .map(|sid| {
                json!({
                    "IdentityStoreArn": identity_store_arn(&req.region, &req.account_id, &sid),
                    "IdentityStoreId": sid,
                })
            })
            .collect();
        let (page, next) = paginate(rows, &b)?;
        let mut out = json!({ "IdentityStores": page });
        if let Some(t) = next {
            out["NextToken"] = json!(t);
        }
        ok(out)
    }

    fn update_identity_store(&self, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let b = parse(req)?;
        let sid = self.existing_store(&b, &req.account_id)?;
        let network = b
            .get("NetworkConfiguration")
            .map(normalize_network_configuration)
            .transpose()?;
        if let Some(nc) = network {
            // Full replacement of the store's network configuration.
            let mut guard = self.state.write();
            let dir = guard
                .get_or_create(&req.account_id)
                .stores
                .entry(sid.clone())
                .or_default();
            dir.network_configuration = Some(nc);
        }
        ok(json!({
            "IdentityStoreId": sid,
            "IdentityStoreArn": identity_store_arn(&req.region, &req.account_id, &sid),
        }))
    }
}

/// Validate a `NetworkConfiguration` against the model and return it with only
/// its modeled members (`VpceAccessRequired` required; the three optional
/// lists each 1..=50 unique items).
fn normalize_network_configuration(nc: &Value) -> Result<Value, AwsServiceError> {
    let obj = nc
        .as_object()
        .ok_or_else(|| validation("NetworkConfiguration must be an object."))?;
    let vpce = obj
        .get("VpceAccessRequired")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            validation("NetworkConfiguration.VpceAccessRequired must be set to true or false.")
        })?;
    let mut out = Map::new();
    out.insert("VpceAccessRequired".into(), json!(vpce));
    for (field, item_ok) in [
        ("ApiRestrictSourceVpcs", is_vpc_id as fn(&str) -> bool),
        ("ApiAllowSourceIps", is_ip_cidr),
        ("ScimAllowSourceIps", is_ip_cidr),
    ] {
        let Some(v) = obj.get(field) else {
            continue;
        };
        let items: Vec<&str> = v
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .filter(|items: &Vec<&str>| Some(items.len()) == v.as_array().map(Vec::len))
            .ok_or_else(|| validation(&format!("{field} must be a list of strings.")))?;
        if !(1..=50).contains(&items.len()) {
            return Err(validation(&format!(
                "{field} must contain between 1 and 50 items, inclusive."
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &items {
            if !item_ok(item) {
                return Err(validation(&format!(
                    "{field} contains an invalid value: {item}"
                )));
            }
            if !seen.insert(*item) {
                return Err(validation(&format!(
                    "{field} must not contain duplicate values: {item}"
                )));
            }
        }
        out.insert(field.into(), json!(items));
    }
    Ok(Value::Object(out))
}

/// `VpcIdType`: `^vpc-([0-9a-f]){8}(([0-9a-f]){9})?$`.
fn is_vpc_id(s: &str) -> bool {
    s.strip_prefix("vpc-").is_some_and(|hex| {
        (hex.len() == 8 || hex.len() == 17)
            && hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// `IpCidrType`: an IPv4 or IPv6 address with an optional in-range prefix
/// length (@length 2..=43).
fn is_ip_cidr(s: &str) -> bool {
    if !(2..=43).contains(&s.len()) {
        return false;
    }
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (s, None),
    };
    let max = match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) if !addr.contains(':') => 32,
        // The model's IPv6 alternative has no embedded-IPv4 (`::ffff:1.2.3.4`)
        // form.
        Ok(std::net::IpAddr::V6(_)) if !addr.contains('.') => 128,
        _ => return false,
    };
    match prefix {
        None => true,
        Some(p) => {
            !p.is_empty()
                && !(p.len() > 1 && p.starts_with('0'))
                && p.bytes().all(|c| c.is_ascii_digit())
                && p.parse::<u32>().is_ok_and(|n| n <= max)
        }
    }
}

/// Extract `(AttributePath, AttributeValue)` from an `AlternateIdentifier`'s
/// `UniqueAttribute` member. `ExternalId` identifiers are not modeled in the
/// directory yet, so they resolve to not-found by the caller.
fn alternate_identifier(b: &Value) -> Result<(String, String), AwsServiceError> {
    let ai = b
        .get("AlternateIdentifier")
        .ok_or_else(|| validation("AlternateIdentifier must be specified."))?;
    if let Some(ua) = ai.get("UniqueAttribute") {
        let path = ua
            .get("AttributePath")
            .and_then(Value::as_str)
            .ok_or_else(|| validation("AttributePath must be specified."))?;
        let val = ua
            .get("AttributeValue")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .ok_or_else(|| validation("AttributeValue must be specified."))?;
        return Ok((path.to_string(), val));
    }
    // ExternalId identifier: nothing indexed -> resolve to a sentinel that
    // never matches, producing a clean ResourceNotFoundException upstream.
    Ok((String::new(), "\u{0}__no_match__".to_string()))
}

/// Does the attribute at `path` (dot-separated, resolved case-insensitively)
/// equal `want`? Descends into nested objects and, for list-valued segments,
/// matches when *any* element satisfies the remaining path — so a
/// `UniqueAttribute` lookup like `Emails.Value` finds the user whose `Emails`
/// list has an element with that `Value`.
fn attribute_matches(value: &Value, path: &str, want: &str) -> bool {
    let mut segs = path.split('.');
    let Some(head) = segs.next() else {
        return false;
    };
    let child = match value {
        Value::Object(m) => m
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(head))
            .map(|(_, v)| v),
        _ => None,
    };
    let Some(child) = child else {
        return false;
    };
    let rest: Vec<&str> = segs.collect();
    if rest.is_empty() {
        return match child {
            Value::String(s) => s == want,
            Value::Array(arr) => arr.iter().any(|e| e.as_str() == Some(want)),
            _ => false,
        };
    }
    let rest_path = rest.join(".");
    match child {
        Value::Array(arr) => arr.iter().any(|e| attribute_matches(e, &rest_path, want)),
        Value::Object(_) => attribute_matches(child, &rest_path, want),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use fakecloud_core::multi_account::MultiAccountState;
    use http::{HeaderMap, Method};
    use parking_lot::{Mutex, RwLock};
    use std::collections::HashMap;

    fn svc() -> IdentityStoreService {
        IdentityStoreService::new(Arc::new(RwLock::new(MultiAccountState::new(
            "000000000000",
            "us-east-1",
            "",
        ))))
    }

    fn req(action: &str, body: Value) -> AwsRequest {
        AwsRequest {
            service: "identitystore".into(),
            action: action.into(),
            region: "us-east-1".into(),
            account_id: "000000000000".into(),
            request_id: "req".into(),
            headers: HeaderMap::new(),
            query_params: HashMap::new(),
            body: Bytes::from(serde_json::to_vec(&body).unwrap()),
            body_stream: Mutex::new(None),
            path_segments: vec![],
            raw_path: String::new(),
            raw_query: String::new(),
            method: Method::POST,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn call(s: &IdentityStoreService, action: &str, body: Value) -> Value {
        let resp = dispatch(s, &req(action, body)).expect("op ok");
        serde_json::from_slice(resp.body.expect_bytes()).unwrap()
    }

    #[test]
    fn user_lifecycle_and_get_by_username() {
        let s = svc();
        let created = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": "d-1234567890", "UserName": "alice", "DisplayName": "Alice" }),
        );
        let uid = created["UserId"].as_str().unwrap().to_string();

        let got = call(
            &s,
            "GetUserId",
            json!({
                "IdentityStoreId": "d-1234567890",
                "AlternateIdentifier": { "UniqueAttribute": { "AttributePath": "UserName", "AttributeValue": "alice" } }
            }),
        );
        assert_eq!(got["UserId"], json!(uid));

        let desc = call(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": "d-1234567890", "UserId": uid }),
        );
        assert_eq!(desc["UserName"], json!("alice"));
        assert_eq!(desc["UserStatus"], json!("ENABLED"));
    }

    #[test]
    fn duplicate_username_conflicts() {
        let s = svc();
        let body = json!({ "IdentityStoreId": "d-1", "UserName": "bob" });
        dispatch(&s, &req("CreateUser", body.clone())).unwrap();
        let err = dispatch(&s, &req("CreateUser", body)).err().unwrap();
        assert_eq!(err.code(), "ConflictException");
        // ConflictException is modeled with @httpError(409), not a generic 400.
        assert_eq!(err.status(), http::StatusCode::CONFLICT);
    }

    #[test]
    fn duplicate_group_and_membership_conflicts_are_409() {
        let s = svc();
        let sid = "d-c";
        let g = call(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": sid, "DisplayName": "grp" }),
        );
        let gid = g["GroupId"].as_str().unwrap().to_string();
        // Duplicate group display name.
        let err = dispatch(
            &s,
            &req(
                "CreateGroup",
                json!({ "IdentityStoreId": sid, "DisplayName": "grp" }),
            ),
        )
        .err()
        .unwrap();
        assert_eq!(err.code(), "ConflictException");
        assert_eq!(err.status(), http::StatusCode::CONFLICT);
        // Duplicate membership.
        let uid = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "mm" }),
        )["UserId"]
            .as_str()
            .unwrap()
            .to_string();
        let body = json!({ "IdentityStoreId": sid, "GroupId": gid, "MemberId": { "UserId": uid } });
        dispatch(&s, &req("CreateGroupMembership", body.clone())).unwrap();
        let err = dispatch(&s, &req("CreateGroupMembership", body))
            .err()
            .unwrap();
        assert_eq!(err.code(), "ConflictException");
        assert_eq!(err.status(), http::StatusCode::CONFLICT);
    }

    #[test]
    fn membership_and_is_member_in_groups() {
        let s = svc();
        let sid = "d-9";
        let u = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "u" }),
        );
        let uid = u["UserId"].as_str().unwrap().to_string();
        let g = call(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": sid, "DisplayName": "g" }),
        );
        let gid = g["GroupId"].as_str().unwrap().to_string();
        call(
            &s,
            "CreateGroupMembership",
            json!({ "IdentityStoreId": sid, "GroupId": gid, "MemberId": { "UserId": uid } }),
        );
        let res = call(
            &s,
            "IsMemberInGroups",
            json!({ "IdentityStoreId": sid, "MemberId": { "UserId": uid }, "GroupIds": [gid, "d-nope"] }),
        );
        let arr = res["Results"].as_array().unwrap();
        assert_eq!(arr[0]["MembershipExists"], json!(true));
        assert_eq!(arr[1]["MembershipExists"], json!(false));
    }

    #[test]
    fn update_user_applies_operations() {
        let s = svc();
        let sid = "d-u";
        let u = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "c" }),
        );
        let uid = u["UserId"].as_str().unwrap().to_string();
        call(
            &s,
            "UpdateUser",
            json!({
                "IdentityStoreId": sid, "UserId": uid,
                "Operations": [{ "AttributePath": "DisplayName", "AttributeValue": "Charlie" }]
            }),
        );
        let desc = call(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": uid }),
        );
        assert_eq!(desc["DisplayName"], json!("Charlie"));
    }

    #[test]
    fn list_users_paginates() {
        let s = svc();
        let sid = "d-p";
        for i in 0..3 {
            call(
                &s,
                "CreateUser",
                json!({ "IdentityStoreId": sid, "UserName": format!("u{i}") }),
            );
        }
        let page1 = call(
            &s,
            "ListUsers",
            json!({ "IdentityStoreId": sid, "MaxResults": 2 }),
        );
        assert_eq!(page1["Users"].as_array().unwrap().len(), 2);
        let token = page1["NextToken"].as_str().unwrap().to_string();
        let page2 = call(
            &s,
            "ListUsers",
            json!({ "IdentityStoreId": sid, "MaxResults": 2, "NextToken": token }),
        );
        assert_eq!(page2["Users"].as_array().unwrap().len(), 1);
        assert!(page2.get("NextToken").is_none());
    }

    #[test]
    fn describe_missing_user_is_not_found() {
        let s = svc();
        let err = dispatch(
            &s,
            &req(
                "DescribeUser",
                json!({ "IdentityStoreId": "d-x", "UserId": "missing" }),
            ),
        )
        .err()
        .unwrap();
        assert_eq!(err.code(), "ResourceNotFoundException");
    }

    #[test]
    fn update_user_camelcase_scim_path_hits_pascalcase_bag() {
        // SCIM clients (Terraform's aws_identitystore_user) address attributes
        // in camelCase; the bag stores PascalCase member names. The update must
        // land on the existing key, not a shadow lowercase one.
        let s = svc();
        let sid = "d-scim";
        let created = call(
            &s,
            "CreateUser",
            json!({
                "IdentityStoreId": sid, "UserName": "j",
                "Name": { "GivenName": "John", "FamilyName": "Doe" },
                "Title": "Mr"
            }),
        );
        let uid = created["UserId"].as_str().unwrap().to_string();
        call(
            &s,
            "UpdateUser",
            json!({
                "IdentityStoreId": sid, "UserId": uid,
                "Operations": [
                    { "AttributePath": "name.givenName", "AttributeValue": "Jane" },
                    { "AttributePath": "title", "AttributeValue": "Ms" }
                ]
            }),
        );
        let desc = call(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": uid }),
        );
        assert_eq!(desc["Name"]["GivenName"], json!("Jane"));
        // The untouched sibling is preserved, not clobbered by a new object.
        assert_eq!(desc["Name"]["FamilyName"], json!("Doe"));
        assert_eq!(desc["Title"], json!("Ms"));
        // No shadow lowercase keys leaked into the bag.
        assert!(desc.get("title").is_none());
        assert!(desc.get("name").is_none());
    }

    #[test]
    fn update_user_list_attribute_canonicalizes_subkeys() {
        // SCIM clients send list-attribute updates with camelCase element keys;
        // the stored bag must expose PascalCase so SDKs read the sub-fields.
        let s = svc();
        let sid = "d-list";
        let uid = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "l" }),
        )["UserId"]
            .as_str()
            .unwrap()
            .to_string();
        call(
            &s,
            "UpdateUser",
            json!({
                "IdentityStoreId": sid, "UserId": uid,
                "Operations": [{
                    "AttributePath": "phoneNumbers",
                    "AttributeValue": [{ "value": "+15551234", "type": "The Type 2", "primary": true }]
                }]
            }),
        );
        let desc = call(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": uid }),
        );
        assert_eq!(desc["PhoneNumbers"][0]["Type"], json!("The Type 2"));
        assert_eq!(desc["PhoneNumbers"][0]["Value"], json!("+15551234"));
        assert_eq!(desc["PhoneNumbers"][0]["Primary"], json!(true));
    }

    #[test]
    fn get_user_id_by_nested_email_value() {
        // `data.aws_identitystore_user` looks users up by `Emails.Value`, a
        // nested list attribute path.
        let s = svc();
        let sid = "d-mail";
        let created = call(
            &s,
            "CreateUser",
            json!({
                "IdentityStoreId": sid, "UserName": "z",
                "Emails": [{ "Value": "z@example.com", "Primary": true }]
            }),
        );
        let uid = created["UserId"].as_str().unwrap().to_string();
        let got = call(
            &s,
            "GetUserId",
            json!({
                "IdentityStoreId": sid,
                "AlternateIdentifier": { "UniqueAttribute": { "AttributePath": "Emails.Value", "AttributeValue": "z@example.com" } }
            }),
        );
        assert_eq!(got["UserId"], json!(uid));
    }

    #[test]
    fn update_group_camelcase_display_name() {
        let s = svc();
        let sid = "d-g";
        let g = call(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": sid, "DisplayName": "old", "Description": "d" }),
        );
        let gid = g["GroupId"].as_str().unwrap().to_string();
        call(
            &s,
            "UpdateGroup",
            json!({
                "IdentityStoreId": sid, "GroupId": gid,
                "Operations": [{ "AttributePath": "displayName", "AttributeValue": "new" }]
            }),
        );
        let desc = call(
            &s,
            "DescribeGroup",
            json!({ "IdentityStoreId": sid, "GroupId": gid }),
        );
        assert_eq!(desc["DisplayName"], json!("new"));
        // GetGroupId reflects the renamed display name.
        let got = call(
            &s,
            "GetGroupId",
            json!({
                "IdentityStoreId": sid,
                "AlternateIdentifier": { "UniqueAttribute": { "AttributePath": "DisplayName", "AttributeValue": "new" } }
            }),
        );
        assert_eq!(got["GroupId"], json!(gid));
    }

    fn call_err(s: &IdentityStoreService, action: &str, body: Value) -> AwsServiceError {
        dispatch(s, &req(action, body))
            .err()
            .expect("op should fail")
    }

    fn field<'a>(err: &'a AwsServiceError, name: &str) -> Option<&'a str> {
        err.extra_fields()
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn user_revision_arn_and_optimistic_concurrency() {
        let s = svc();
        let sid = "d-1234567890";
        let created = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "rev" }),
        );
        let uid = created["UserId"].as_str().unwrap().to_string();
        // Non-legacy stores mint `<10 hex of the store id>-<UUID>` ids.
        assert!(uid.starts_with("1234567890-"), "{uid}");
        assert_eq!(uid.len(), 47);
        let arn = format!("arn:aws:identitystore:::user/{uid}");
        assert_eq!(created["UserArn"], json!(arn));
        assert_eq!(created["Revision"], json!("1"));

        let desc = call(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": uid }),
        );
        assert_eq!(desc["UserArn"], json!(arn));
        assert_eq!(desc["Revision"], json!("1"));
        let got = call(
            &s,
            "GetUserId",
            json!({
                "IdentityStoreId": sid,
                "AlternateIdentifier": { "UniqueAttribute": { "AttributePath": "UserName", "AttributeValue": "rev" } }
            }),
        );
        assert_eq!(got["UserArn"], json!(arn));

        let ops = json!([{ "AttributePath": "DisplayName", "AttributeValue": "R" }]);
        let updated = call(
            &s,
            "UpdateUser",
            json!({ "IdentityStoreId": sid, "UserId": uid, "Operations": ops, "Revision": "1" }),
        );
        assert_eq!(
            updated,
            json!({ "IdentityStoreId": sid, "UserId": uid, "UserArn": arn, "Revision": "2" })
        );
        let listed = call(&s, "ListUsers", json!({ "IdentityStoreId": sid }));
        assert_eq!(listed["Users"][0]["Revision"], json!("2"));
        assert_eq!(listed["Users"][0]["UserArn"], json!(arn));

        // A stale revision is rejected and leaves the user untouched.
        let err = call_err(
            &s,
            "UpdateUser",
            json!({ "IdentityStoreId": sid, "UserId": uid, "Operations": ops, "Revision": "1" }),
        );
        assert_eq!(err.code(), "ConflictException");
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(field(&err, "Reason"), Some("CONCURRENT_MODIFICATION"));
        let err = call_err(
            &s,
            "DeleteUser",
            json!({ "IdentityStoreId": sid, "UserId": uid, "Revision": "1" }),
        );
        assert_eq!(field(&err, "Reason"), Some("CONCURRENT_MODIFICATION"));
        // A malformed revision is a validation error, not a conflict.
        let err = call_err(
            &s,
            "DeleteUser",
            json!({ "IdentityStoreId": sid, "UserId": uid, "Revision": "abc" }),
        );
        assert_eq!(err.code(), "ValidationException");
        // The matching revision deletes.
        call(
            &s,
            "DeleteUser",
            json!({ "IdentityStoreId": sid, "UserId": uid, "Revision": "2" }),
        );
        let err = call_err(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": uid }),
        );
        assert_eq!(err.code(), "ResourceNotFoundException");
        // Revision is validated even when the target no longer exists.
        for bad in ["", "1".repeat(65).as_str()] {
            let err = call_err(
                &s,
                "DeleteUser",
                json!({ "IdentityStoreId": sid, "UserId": uid, "Revision": bad }),
            );
            assert_eq!(err.code(), "ValidationException");
        }
    }

    #[test]
    fn group_and_membership_arns_and_group_revision() {
        let s = svc();
        let sid = "d-abcdef0123";
        let g = call(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": sid, "DisplayName": "eng" }),
        );
        let gid = g["GroupId"].as_str().unwrap().to_string();
        let garn = format!("arn:aws:identitystore:::group/{gid}");
        assert_eq!(g["GroupArn"], json!(garn));
        assert_eq!(g["Revision"], json!("1"));
        let updated = call(
            &s,
            "UpdateGroup",
            json!({
                "IdentityStoreId": sid, "GroupId": gid,
                "Operations": [{ "AttributePath": "Description", "AttributeValue": "d" }]
            }),
        );
        assert_eq!(
            updated,
            json!({ "GroupId": gid, "IdentityStoreId": sid, "GroupArn": garn, "Revision": "2" })
        );
        let err = call_err(
            &s,
            "UpdateGroup",
            json!({
                "IdentityStoreId": sid, "GroupId": gid, "Revision": "1",
                "Operations": [{ "AttributePath": "Description", "AttributeValue": "x" }]
            }),
        );
        assert_eq!(field(&err, "Reason"), Some("CONCURRENT_MODIFICATION"));
        let desc = call(
            &s,
            "DescribeGroup",
            json!({ "IdentityStoreId": sid, "GroupId": gid }),
        );
        assert_eq!(desc["Description"], json!("d"));
        assert_eq!(desc["Revision"], json!("2"));
        let got = call(
            &s,
            "GetGroupId",
            json!({
                "IdentityStoreId": sid,
                "AlternateIdentifier": { "UniqueAttribute": { "AttributePath": "DisplayName", "AttributeValue": "eng" } }
            }),
        );
        assert_eq!(got["GroupArn"], json!(garn));
        let listed = call(&s, "ListGroups", json!({ "IdentityStoreId": sid }));
        assert_eq!(listed["Groups"][0]["GroupArn"], json!(garn));

        let uid = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "m" }),
        )["UserId"]
            .as_str()
            .unwrap()
            .to_string();
        let m = call(
            &s,
            "CreateGroupMembership",
            json!({ "IdentityStoreId": sid, "GroupId": gid, "MemberId": { "UserId": uid } }),
        );
        let mid = m["MembershipId"].as_str().unwrap().to_string();
        assert!(mid.starts_with("abcdef0123-"), "{mid}");
        let marn = format!("arn:aws:identitystore:::membership/{mid}");
        assert_eq!(m["MembershipArn"], json!(marn));
        let d = call(
            &s,
            "DescribeGroupMembership",
            json!({ "IdentityStoreId": sid, "MembershipId": mid }),
        );
        assert_eq!(d["MembershipArn"], json!(marn));
        let got = call(
            &s,
            "GetGroupMembershipId",
            json!({ "IdentityStoreId": sid, "GroupId": gid, "MemberId": { "UserId": uid } }),
        );
        assert_eq!(got["MembershipArn"], json!(marn));
        let l = call(
            &s,
            "ListGroupMemberships",
            json!({ "IdentityStoreId": sid, "GroupId": gid }),
        );
        assert_eq!(l["GroupMemberships"][0]["MembershipArn"], json!(marn));
        let l = call(
            &s,
            "ListGroupMembershipsForMember",
            json!({ "IdentityStoreId": sid, "MemberId": { "UserId": uid } }),
        );
        assert_eq!(l["GroupMemberships"][0]["MembershipArn"], json!(marn));
        // Duplicate display names carry the uniqueness reason.
        let err = call_err(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": sid, "DisplayName": "eng" }),
        );
        assert_eq!(
            field(&err, "Reason"),
            Some("UNIQUENESS_CONSTRAINT_VIOLATION")
        );
    }

    #[test]
    fn identity_store_id_accepts_arn() {
        let s = svc();
        let arn = "arn:aws:identitystore::000000000000:identitystore/d-1234567890";
        let created = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": arn, "UserName": "a" }),
        );
        // Responses carry the bare id; the ARN resolves to the same directory.
        assert_eq!(created["IdentityStoreId"], json!("d-1234567890"));
        let listed = call(
            &s,
            "ListUsers",
            json!({ "IdentityStoreId": "d-1234567890" }),
        );
        assert_eq!(listed["Users"].as_array().unwrap().len(), 1);
        let err = call_err(
            &s,
            "ListUsers",
            json!({ "IdentityStoreId": "arn:aws:s3:::bucket" }),
        );
        assert_eq!(err.code(), "ValidationException");
    }

    #[test]
    fn identity_store_arn_of_another_account_is_not_found() {
        let s = svc();
        call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": "d-1234567890", "UserName": "a" }),
        );
        let other = "arn:aws:identitystore::111122223333:identitystore/d-1234567890";
        let err = call_err(&s, "ListUsers", json!({ "IdentityStoreId": other }));
        assert_eq!(err.code(), "ResourceNotFoundException");
        assert_eq!(field(&err, "ResourceType"), Some("IDENTITY_STORE"));
        // An account-less store ARN is malformed.
        let err = call_err(
            &s,
            "ListUsers",
            json!({ "IdentityStoreId": "arn:aws:identitystore:::identitystore/d-1234567890" }),
        );
        assert_eq!(err.code(), "ValidationException");
    }

    #[test]
    fn resource_ids_accept_arn_form() {
        let s = svc();
        let sid = "d-1234567890";
        let uid = call(
            &s,
            "CreateUser",
            json!({ "IdentityStoreId": sid, "UserName": "u" }),
        )["UserId"]
            .as_str()
            .unwrap()
            .to_string();
        let gid = call(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": sid, "DisplayName": "g" }),
        )["GroupId"]
            .as_str()
            .unwrap()
            .to_string();
        let uarn = format!("arn:aws:identitystore:::user/{uid}");
        let garn = format!("arn:aws:identitystore:::group/{gid}");
        // 81 chars: over the old 47 cap, within the model's 100.
        assert!(uarn.len() > 47 && uarn.len() <= 100);

        let desc = call(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": uarn }),
        );
        assert_eq!(desc["UserId"], json!(uid));

        let mid = call(
            &s,
            "CreateGroupMembership",
            json!({ "IdentityStoreId": sid, "GroupId": garn, "MemberId": { "UserId": uarn } }),
        )["MembershipId"]
            .as_str()
            .unwrap()
            .to_string();
        let m = call(
            &s,
            "DescribeGroupMembership",
            json!({
                "IdentityStoreId": sid,
                "MembershipId": format!("arn:aws:identitystore:::membership/{mid}"),
            }),
        );
        // Stored under the bare ids, not the ARNs.
        assert_eq!(m["GroupId"], json!(gid));
        assert_eq!(m["MemberId"]["UserId"], json!(uid));

        let res = call(
            &s,
            "IsMemberInGroups",
            json!({ "IdentityStoreId": sid, "MemberId": { "UserId": uarn }, "GroupIds": [garn] }),
        );
        assert_eq!(res["Results"][0]["MembershipExists"], json!(true));

        // An ARN of the wrong resource type is rejected.
        let err = call_err(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": garn }),
        );
        assert_eq!(err.code(), "ValidationException");
        let err = call_err(
            &s,
            "IsMemberInGroups",
            json!({ "IdentityStoreId": sid, "MemberId": { "UserId": uid }, "GroupIds": [uarn] }),
        );
        assert_eq!(err.code(), "ValidationException");
        // A slash inside the id is not a valid resource ARN.
        let err = call_err(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": format!("{uarn}/x") }),
        );
        assert_eq!(err.code(), "ValidationException");
        // Over the model's 100-char cap.
        let err = call_err(
            &s,
            "DescribeUser",
            json!({ "IdentityStoreId": sid, "UserId": "a".repeat(101) }),
        );
        assert_eq!(err.code(), "ValidationException");
    }

    #[test]
    fn identity_store_describe_list_update() {
        let s = svc().with_instance_store_lookup(Arc::new(|account: &str| {
            if account == "000000000000" {
                vec!["d-aaaaaaaaaa".to_string()]
            } else {
                vec![]
            }
        }));
        // A directory created by a write is also a known store.
        call(
            &s,
            "CreateGroup",
            json!({ "IdentityStoreId": "d-bbbbbbbbbb", "DisplayName": "g" }),
        );
        let listed = call(&s, "ListIdentityStores", json!({}));
        assert_eq!(
            listed,
            json!({ "IdentityStores": [
                { "IdentityStoreId": "d-aaaaaaaaaa", "IdentityStoreArn": "arn:aws:identitystore::000000000000:identitystore/d-aaaaaaaaaa" },
                { "IdentityStoreId": "d-bbbbbbbbbb", "IdentityStoreArn": "arn:aws:identitystore::000000000000:identitystore/d-bbbbbbbbbb" },
            ]})
        );
        let page = call(&s, "ListIdentityStores", json!({ "MaxResults": 1 }));
        assert_eq!(page["IdentityStores"].as_array().unwrap().len(), 1);
        assert_eq!(page["NextToken"], json!("1"));

        let arn = "arn:aws:identitystore::000000000000:identitystore/d-aaaaaaaaaa";
        let desc = call(
            &s,
            "DescribeIdentityStore",
            json!({ "IdentityStoreId": arn }),
        );
        assert_eq!(
            desc,
            json!({ "IdentityStoreId": "d-aaaaaaaaaa", "IdentityStoreArn": arn })
        );

        let updated = call(
            &s,
            "UpdateIdentityStore",
            json!({
                "IdentityStoreId": "d-aaaaaaaaaa",
                "NetworkConfiguration": {
                    "VpceAccessRequired": true,
                    "ApiRestrictSourceVpcs": ["vpc-0123abcd"],
                    "ScimAllowSourceIps": ["0.0.0.0/0", "2001:db8::/32"]
                }
            }),
        );
        assert_eq!(
            updated,
            json!({ "IdentityStoreId": "d-aaaaaaaaaa", "IdentityStoreArn": arn })
        );
        let desc = call(
            &s,
            "DescribeIdentityStore",
            json!({ "IdentityStoreId": "d-aaaaaaaaaa" }),
        );
        assert_eq!(
            desc["NetworkConfiguration"],
            json!({
                "VpceAccessRequired": true,
                "ApiRestrictSourceVpcs": ["vpc-0123abcd"],
                "ScimAllowSourceIps": ["0.0.0.0/0", "2001:db8::/32"]
            })
        );
        // Full replacement: a later update drops the omitted lists.
        call(
            &s,
            "UpdateIdentityStore",
            json!({
                "IdentityStoreId": "d-aaaaaaaaaa",
                "NetworkConfiguration": { "VpceAccessRequired": false }
            }),
        );
        let desc = call(
            &s,
            "DescribeIdentityStore",
            json!({ "IdentityStoreId": "d-aaaaaaaaaa" }),
        );
        assert_eq!(
            desc["NetworkConfiguration"],
            json!({ "VpceAccessRequired": false })
        );

        for bad in [
            json!({}),
            json!({ "VpceAccessRequired": true, "ApiRestrictSourceVpcs": ["vpc-xyz"] }),
            json!({ "VpceAccessRequired": true, "ApiAllowSourceIps": ["10.0.0.0/33"] }),
            json!({ "VpceAccessRequired": true, "ApiAllowSourceIps": [] }),
            json!({ "VpceAccessRequired": true, "ApiAllowSourceIps": ["1.2.3.4", "1.2.3.4"] }),
        ] {
            let err = call_err(
                &s,
                "UpdateIdentityStore",
                json!({ "IdentityStoreId": "d-aaaaaaaaaa", "NetworkConfiguration": bad }),
            );
            assert_eq!(err.code(), "ValidationException", "{bad}");
        }

        for op in ["DescribeIdentityStore", "UpdateIdentityStore"] {
            let err = call_err(&s, op, json!({ "IdentityStoreId": "d-cccccccccc" }));
            assert_eq!(err.code(), "ResourceNotFoundException");
            assert_eq!(err.status(), StatusCode::NOT_FOUND);
            assert_eq!(field(&err, "ResourceType"), Some("IDENTITY_STORE"));
        }
    }

    #[test]
    fn cidr_and_vpc_patterns() {
        for ok in [
            "10.0.0.0/8",
            "1.2.3.4",
            "0.0.0.0/0",
            "::/0",
            "2001:db8::1",
            "fe80::/10",
        ] {
            assert!(is_ip_cidr(ok), "{ok}");
        }
        for bad in [
            "10.0.0.0/",
            "10.0.0.256",
            "01.2.3.4",
            "1.2.3.4/033",
            "::ffff:1.2.3.4",
            "x",
        ] {
            assert!(!is_ip_cidr(bad), "{bad}");
        }
        assert!(is_vpc_id("vpc-0123abcd"));
        assert!(is_vpc_id("vpc-0123456789abcdef0"));
        assert!(!is_vpc_id("vpc-0123ABCD"));
        assert!(!is_vpc_id("vpc-0123abc"));
    }
}
