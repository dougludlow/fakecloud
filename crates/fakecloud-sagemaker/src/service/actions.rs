//! Stateful SageMaker Action verbs the generic Action arm used to accept and
//! discard: training-plan extensions, HyperPod node volume attachments, cluster
//! health checks and edge deployment stage transitions. Each resolves its
//! target against stored state (`ResourceNotFound` when absent), persists the
//! mutation and reflects it in the sibling Describe / history reads.

use serde_json::{json, Map, Value};

use fakecloud_core::service::{AwsResponse, AwsServiceError};

use crate::generated::OpMeta;

use super::special::{cluster_aliases, find_cluster_node, str_member, CLUSTER_NODE_FAMILY};
use super::{engine, missing, not_found, now_epoch, ok_json, Ctx, SageMakerService};

// ── Training plan extensions ─────────────────────────────────────────────

const TRAINING_PLAN_FAMILY: &str = "TrainingPlan";
/// Extension offerings minted by `SearchTrainingPlanOfferings(TrainingPlanArn)`,
/// keyed by offering id, consumed by `ExtendTrainingPlan`.
const EXTENSION_OFFERING_FAMILY: &str = "TrainingPlanExtensionOffering";
/// Internal training-plan record member: the applied extensions, oldest first.
/// Not a `DescribeTrainingPlan` output member, so projections drop it.
const PLAN_EXTENSIONS: &str = "__Extensions";
/// Default extension length offered when the search names no duration.
const DEFAULT_EXTENSION_HOURS: i64 = 24;
/// `TrainingPlanExtensionDurationHours` upper bound.
const MAX_EXTENSION_HOURS: i64 = 4368;

fn resolve_plan(
    data: &crate::state::SageMakerData,
    meta: &OpMeta,
    plan: &str,
) -> Result<String, AwsServiceError> {
    data.resolve_key(TRAINING_PLAN_FAMILY, plan)
        .ok_or_else(|| missing(meta, format!("Training plan '{plan}' does not exist.")))
}

fn as_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `SearchTrainingPlanOfferings`. With a `TrainingPlanArn` it searches for
/// *extension* offerings of that plan: the plan must exist, and one offering
/// starting where the plan currently ends (for the requested `DurationHours`)
/// is minted and persisted so `ExtendTrainingPlan` can redeem it. Without a
/// plan ARN it is a new-plan offering search, answered by the generic action
/// projection.
pub(super) fn search_training_plan_offerings(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let Some(plan_arn) = body.get("TrainingPlanArn").and_then(Value::as_str) else {
        return Ok((engine::action(ctx, meta, body), false));
    };
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let key = resolve_plan(data, meta, plan_arn)?;
    let plan = data
        .get_resource(TRAINING_PLAN_FAMILY, &key)
        .cloned()
        .unwrap_or(Value::Null);
    let plan_arn = plan
        .get("TrainingPlanArn")
        .and_then(Value::as_str)
        .unwrap_or(plan_arn)
        .to_string();
    let hours = body
        .get("DurationHours")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_EXTENSION_HOURS)
        .clamp(1, MAX_EXTENSION_HOURS);
    let now = now_epoch().as_f64().unwrap_or_default();
    let start = as_f64(plan.get("EndTime")).unwrap_or(now).max(now);
    let end = start + (hours * 3600) as f64;
    let seq = data.next_seq();
    let id = format!(
        "tpeo-{}",
        super::mint_id(&ctx.account, EXTENSION_OFFERING_FAMILY, &seq.to_string()).replace('-', "")
    );
    let offering = json!({
        "TrainingPlanExtensionOfferingId": id,
        "AvailabilityZone": format!("{}a", ctx.region),
        "StartDate": start,
        "EndDate": end,
        "DurationHours": hours,
        "UpfrontFee": "0",
        "CurrencyCode": "USD",
    });
    let mut stored = offering.clone();
    stored["TrainingPlanArn"] = Value::String(plan_arn);
    data.put_resource(EXTENSION_OFFERING_FAMILY, &id, stored);

    let mut out = Map::new();
    out.insert(
        "TrainingPlanOfferings".to_string(),
        Value::Array(Vec::new()),
    );
    out.insert(
        "TrainingPlanExtensionOfferings".to_string(),
        Value::Array(vec![offering]),
    );
    Ok((ok_json(Value::Object(out)), true))
}

/// `ExtendTrainingPlan`: redeem an extension offering: the offering and its
/// plan must exist (`ResourceNotFound` otherwise). The plan's `EndTime` and
/// `DurationHours` grow by the offering's duration, the extension is appended
/// to the plan's history (`DescribeTrainingPlanExtensionHistory`) and the
/// offering is consumed.
pub(super) fn extend_training_plan(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let offering_id = str_member(body, "TrainingPlanExtensionOfferingId");
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let offering = data
        .get_resource(EXTENSION_OFFERING_FAMILY, &offering_id)
        .cloned()
        .ok_or_else(|| {
            not_found(format!(
                "Training plan extension offering '{offering_id}' does not exist."
            ))
        })?;
    let plan_arn = offering
        .get("TrainingPlanArn")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let key = resolve_plan(data, meta, &plan_arn)?;
    let hours = offering
        .get("DurationHours")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut extension = Map::new();
    for k in [
        "TrainingPlanExtensionOfferingId",
        "StartDate",
        "EndDate",
        "DurationHours",
        "AvailabilityZone",
        "UpfrontFee",
        "CurrencyCode",
    ] {
        if let Some(v) = offering.get(k) {
            extension.insert(k.to_string(), v.clone());
        }
    }
    extension.insert("ExtendedAt".to_string(), now_epoch());
    extension.insert("Status".to_string(), Value::String("Scheduled".to_string()));
    extension.insert(
        "PaymentStatus".to_string(),
        Value::String("Completed".to_string()),
    );
    let extension = Value::Object(extension);

    if let Some(plan) = data
        .get_resource_mut(TRAINING_PLAN_FAMILY, &key)
        .and_then(Value::as_object_mut)
    {
        let mut history = plan
            .get(PLAN_EXTENSIONS)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        history.push(extension.clone());
        plan.insert(PLAN_EXTENSIONS.to_string(), Value::Array(history));
        if let Some(end) = offering.get("EndDate").cloned() {
            plan.insert("EndTime".to_string(), end);
        }
        let duration = plan
            .get("DurationHours")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        plan.insert("DurationHours".to_string(), json!(duration + hours));
        plan.insert("LastModifiedTime".to_string(), now_epoch());
    }
    data.remove_resource(EXTENSION_OFFERING_FAMILY, &offering_id);

    let mut out = Map::new();
    out.insert(
        "TrainingPlanExtensions".to_string(),
        Value::Array(vec![extension]),
    );
    Ok((ok_json(Value::Object(out)), true))
}

/// `DescribeTrainingPlanExtensionHistory`: the extensions applied to a plan,
/// oldest first, paginated. A missing plan is `ResourceNotFound`.
pub(super) fn describe_training_plan_extension_history(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let plan_arn = str_member(body, "TrainingPlanArn");
    let g = svc.state.read();
    let empty = crate::state::SageMakerData::default();
    let data = g.get(&ctx.account).unwrap_or(&empty);
    let key = resolve_plan(data, meta, &plan_arn)?;
    let history = data
        .get_resource(TRAINING_PLAN_FAMILY, &key)
        .and_then(|p| p.get(PLAN_EXTENSIONS))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let page_size = body
        .get("MaxResults")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(100) as usize;
    let start = body
        .get("NextToken")
        .and_then(Value::as_str)
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(history.len());
    let end = (start + page_size).min(history.len());
    let mut out = Map::new();
    out.insert(
        "TrainingPlanExtensions".to_string(),
        Value::Array(history[start..end].to_vec()),
    );
    if end < history.len() {
        out.insert("NextToken".to_string(), Value::String(end.to_string()));
    }
    Ok((ok_json(Value::Object(out)), false))
}

// ── HyperPod node volumes ────────────────────────────────────────────────

/// Internal node-record member holding the node's attached EBS volumes as
/// `{VolumeId, DeviceName, AttachTime}` objects. Not a `ClusterNodeDetails` /
/// `ClusterNodeSummary` field, so output projections drop it.
const VOLUME_ATTACHMENTS: &str = "__VolumeAttachments";

/// Device names handed out to attached volumes, in order (`/dev/sdf`..`/dev/sdp`,
/// all valid `VolumeDeviceName`s).
const DEVICE_LETTERS: &str = "fghijklmnop";

fn volume_attachments(rec: &Value) -> Vec<Value> {
    rec.get(VOLUME_ATTACHMENTS)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn volume_id_of(a: &Value) -> Option<&str> {
    a.get("VolumeId").and_then(Value::as_str)
}

fn volume_response(
    cluster_arn: String,
    node_id: String,
    attachment: &Value,
    status: &str,
) -> AwsResponse {
    let mut out = Map::new();
    out.insert("ClusterArn".to_string(), Value::String(cluster_arn));
    out.insert("NodeId".to_string(), Value::String(node_id));
    for k in ["VolumeId", "AttachTime", "DeviceName"] {
        if let Some(v) = attachment.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    out.insert("Status".to_string(), Value::String(status.to_string()));
    ok_json(Value::Object(out))
}

/// `AttachClusterNodeVolume`: attach an EBS volume to a node of a HyperPod
/// cluster. The node must exist in the cluster (`ResourceNotFound`); a volume
/// already attached to any node is a `ConflictException`. The attachment is
/// persisted on the node with the next free device name.
pub(super) fn attach_cluster_node_volume(
    svc: &SageMakerService,
    ctx: &Ctx,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let cluster = str_member(body, "ClusterArn");
    let node_id = str_member(body, "NodeId");
    let volume_id = str_member(body, "VolumeId");
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let (aliases, cluster_arn) = cluster_aliases(data, ctx, &cluster);
    let node_key = find_cluster_node(data, &aliases, &node_id).ok_or_else(|| {
        not_found(format!(
            "Node '{node_id}' does not exist in cluster '{cluster}'."
        ))
    })?;
    let in_use = data
        .list_resource_entries(CLUSTER_NODE_FAMILY)
        .iter()
        .any(|(_, rec)| {
            volume_attachments(rec)
                .iter()
                .any(|a| volume_id_of(a) == Some(volume_id.as_str()))
        });
    if in_use {
        return Err(AwsServiceError::aws_error(
            http::StatusCode::CONFLICT,
            "ConflictException",
            format!("Volume '{volume_id}' is already attached to a node."),
        ));
    }
    let node = data
        .get_resource_mut(CLUSTER_NODE_FAMILY, &node_key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| not_found(format!("Node '{node_id}' does not exist.")))?;
    let mut attachments = node
        .get(VOLUME_ATTACHMENTS)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let used: Vec<String> = attachments
        .iter()
        .filter_map(|a| a.get("DeviceName").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let device = DEVICE_LETTERS
        .chars()
        .map(|c| format!("/dev/sd{c}"))
        .find(|d| !used.contains(d))
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                http::StatusCode::BAD_REQUEST,
                "ResourceLimitExceeded",
                format!("Node '{node_id}' has no free device name for another volume."),
            )
        })?;
    let attachment = json!({
        "VolumeId": volume_id,
        "DeviceName": device,
        "AttachTime": now_epoch(),
    });
    attachments.push(attachment.clone());
    node.insert(VOLUME_ATTACHMENTS.to_string(), Value::Array(attachments));
    Ok((
        volume_response(cluster_arn, node_id, &attachment, "ATTACHING"),
        true,
    ))
}

/// `DetachClusterNodeVolume`: detach a volume from a node. The node must exist
/// in the cluster and the volume must be attached to it (`ResourceNotFound`
/// otherwise). Echoes the attachment's device name and attach time.
pub(super) fn detach_cluster_node_volume(
    svc: &SageMakerService,
    ctx: &Ctx,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let cluster = str_member(body, "ClusterArn");
    let node_id = str_member(body, "NodeId");
    let volume_id = str_member(body, "VolumeId");
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let (aliases, cluster_arn) = cluster_aliases(data, ctx, &cluster);
    let node_key = find_cluster_node(data, &aliases, &node_id).ok_or_else(|| {
        not_found(format!(
            "Node '{node_id}' does not exist in cluster '{cluster}'."
        ))
    })?;
    let node = data
        .get_resource_mut(CLUSTER_NODE_FAMILY, &node_key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| not_found(format!("Node '{node_id}' does not exist.")))?;
    let mut attachments = node
        .get(VOLUME_ATTACHMENTS)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(pos) = attachments
        .iter()
        .position(|a| volume_id_of(a) == Some(volume_id.as_str()))
    else {
        return Err(not_found(format!(
            "Volume '{volume_id}' is not attached to node '{node_id}'."
        )));
    };
    let attachment = attachments.remove(pos);
    node.insert(VOLUME_ATTACHMENTS.to_string(), Value::Array(attachments));
    Ok((
        volume_response(cluster_arn, node_id, &attachment, "DETACHING"),
        true,
    ))
}

/// `StartClusterHealthCheck`: the cluster (by name or ARN) must exist
/// (`ResourceNotFound` otherwise); echoes its ARN.
pub(super) fn start_cluster_health_check(
    svc: &SageMakerService,
    ctx: &Ctx,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let cluster = str_member(body, "ClusterName");
    let g = svc.state.read();
    let arn = g
        .get(&ctx.account)
        .and_then(|d| {
            let key = d.resolve_key("Cluster", &cluster)?;
            d.get_resource("Cluster", &key)?
                .get("ClusterArn")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| not_found(format!("Cluster '{cluster}' does not exist.")))?;
    Ok((ok_json(json!({ "ClusterArn": arn })), false))
}

// ── Edge deployment stages ───────────────────────────────────────────────

const EDGE_PLAN_FAMILY: &str = "EdgeDeploymentPlan";

/// The stored plan key for `plan`, or the op's not-found error.
fn resolve_edge_plan(
    data: &crate::state::SageMakerData,
    meta: &OpMeta,
    plan: &str,
) -> Result<String, AwsServiceError> {
    data.resolve_key(EDGE_PLAN_FAMILY, plan).ok_or_else(|| {
        missing(
            meta,
            format!("Edge deployment plan '{plan}' does not exist."),
        )
    })
}

/// The plan's `Stages` list (the one store every stage operation and
/// `DescribeEdgeDeploymentPlan` share), created empty if absent.
fn plan_stages<'a>(
    data: &'a mut crate::state::SageMakerData,
    key: &str,
) -> Option<&'a mut Vec<Value>> {
    let obj = data
        .get_resource_mut(EDGE_PLAN_FAMILY, key)
        .and_then(Value::as_object_mut)?;
    if !obj.get("Stages").is_some_and(Value::is_array) {
        obj.insert("Stages".to_string(), Value::Array(Vec::new()));
    }
    obj.get_mut("Stages").and_then(Value::as_array_mut)
}

fn stage_name(stage: &Value) -> Option<&str> {
    stage.get("StageName").and_then(Value::as_str)
}

/// `CreateEdgeDeploymentStage`: append stages to an existing plan's `Stages`,
/// so `DescribeEdgeDeploymentPlan`, Start / Stop and Delete all see them. An
/// unknown plan, or a stage name the plan already has, is rejected.
pub(super) fn create_edge_deployment_stage(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let plan = str_member(body, "EdgeDeploymentPlanName");
    let incoming = body
        .get("Stages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let key = resolve_edge_plan(data, meta, &plan)?;
    let stages = plan_stages(data, &key).ok_or_else(|| {
        missing(
            meta,
            format!("Edge deployment plan '{plan}' does not exist."),
        )
    })?;
    let mut names: Vec<String> = stages
        .iter()
        .filter_map(stage_name)
        .map(str::to_string)
        .collect();
    for st in &incoming {
        let name = stage_name(st).unwrap_or_default().to_string();
        if names.contains(&name) {
            return Err(AwsServiceError::aws_error(
                http::StatusCode::BAD_REQUEST,
                crate::validate::VALIDATION_ERROR,
                format!("Stage '{name}' already exists in edge deployment plan '{plan}'."),
            ));
        }
        names.push(name);
    }
    stages.extend(incoming);
    if let Some(obj) = data
        .get_resource_mut(EDGE_PLAN_FAMILY, &key)
        .and_then(Value::as_object_mut)
    {
        obj.insert("LastModifiedTime".to_string(), now_epoch());
    }
    Ok((ok_json(Value::Object(Map::new())), true))
}

/// `DeleteEdgeDeploymentStage`: remove one named stage from the plan. An
/// unknown plan or stage is rejected.
pub(super) fn delete_edge_deployment_stage(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let plan = str_member(body, "EdgeDeploymentPlanName");
    let stage = str_member(body, "StageName");
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let key = resolve_edge_plan(data, meta, &plan)?;
    let stages = plan_stages(data, &key).ok_or_else(|| {
        missing(
            meta,
            format!("Edge deployment plan '{plan}' does not exist."),
        )
    })?;
    let Some(pos) = stages
        .iter()
        .position(|s| stage_name(s) == Some(stage.as_str()))
    else {
        return Err(missing(
            meta,
            format!("Stage '{stage}' does not exist in edge deployment plan '{plan}'."),
        ));
    };
    stages.remove(pos);
    Ok((ok_json(Value::Object(Map::new())), true))
}

/// `StartEdgeDeploymentStage` / `StopEdgeDeploymentStage`: the plan and the
/// named stage must exist (the op's not-found error otherwise). The stage's
/// `DeploymentStatus.StageStatus` moves to `DEPLOYED` (start) or `STOPPED`
/// (stop), which `DescribeEdgeDeploymentPlan` reports.
pub(super) fn edge_deployment_stage_transition(
    svc: &SageMakerService,
    ctx: &Ctx,
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(AwsResponse, bool), AwsServiceError> {
    let plan = str_member(body, "EdgeDeploymentPlanName");
    let stage = str_member(body, "StageName");
    let status = if meta.op.starts_with("Start") {
        "DEPLOYED"
    } else {
        "STOPPED"
    };
    let mut g = svc.state.write();
    let data = g.get_or_create(&ctx.account);
    let key = resolve_edge_plan(data, meta, &plan)?;
    let not_found_stage = || {
        missing(
            meta,
            format!("Stage '{stage}' does not exist in edge deployment plan '{plan}'."),
        )
    };
    let stages = plan_stages(data, &key).ok_or_else(not_found_stage)?;
    let obj = stages
        .iter_mut()
        .find(|s| stage_name(s) == Some(stage.as_str()))
        .and_then(Value::as_object_mut)
        .ok_or_else(not_found_stage)?;
    let mut ds = obj
        .get("DeploymentStatus")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ds.insert("StageStatus".to_string(), Value::String(status.to_string()));
    ds.entry("EdgeDeploymentSuccessInStage".to_string())
        .or_insert(json!(0));
    ds.entry("EdgeDeploymentPendingInStage".to_string())
        .or_insert(json!(0));
    ds.entry("EdgeDeploymentFailedInStage".to_string())
        .or_insert(json!(0));
    if status == "DEPLOYED" {
        ds.insert("EdgeDeploymentStageStartTime".to_string(), now_epoch());
    }
    obj.insert("DeploymentStatus".to_string(), Value::Object(ds));
    Ok((ok_json(Value::Object(Map::new())), true))
}
