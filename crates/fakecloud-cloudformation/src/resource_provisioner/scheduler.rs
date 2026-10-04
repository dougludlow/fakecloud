//! `AWS::Scheduler::Schedule` and `AWS::Scheduler::ScheduleGroup`, created,
//! updated and deleted through the EventBridge Scheduler handlers (the ones
//! the API dispatches to), so a stack's schedule exists in Scheduler and the
//! ticker fires it at its target.

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use fakecloud_core::service::AwsRequest;
use fakecloud_scheduler::SchedulerService;
use http::{HeaderMap, Method};
use serde_json::Value;

use super::{ProvisionResult, ResourceDefinition, ResourceProvisioner, StackResource};

/// The schedule group a schedule ARN
/// (`arn:...:scheduler:<region>:<account>:schedule/<group>/<name>`) names.
fn group_of_schedule_arn(arn: &str) -> Option<&str> {
    arn.split_once(":schedule/")?.1.split('/').next()
}

impl ResourceProvisioner {
    fn scheduler_dispatch(
        &self,
        action: &str,
        name: &str,
        body: Value,
        query: HashMap<String, String>,
    ) -> Result<Value, String> {
        let req = AwsRequest {
            service: "scheduler".to_string(),
            action: action.to_string(),
            region: self.region.clone(),
            account_id: self.account_id.clone(),
            request_id: "cfn".to_string(),
            headers: HeaderMap::new(),
            query_params: query,
            body: Bytes::from(body.to_string()),
            body_stream: parking_lot::Mutex::new(None),
            path_segments: Vec::new(),
            raw_path: "/".to_string(),
            raw_query: String::new(),
            method: Method::POST,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        };
        let resp = SchedulerService::new(self.scheduler_state.clone())
            .provision_sync(action, name, &req)
            .map_err(|e| format!("{}: {}", e.code(), e.message()))?;
        Ok(serde_json::from_slice(resp.body.expect_bytes()).unwrap_or(Value::Null))
    }

    /// The CreateSchedule / UpdateSchedule body a template's properties
    /// describe (the CFN property names are the API's).
    fn schedule_body(props: &Value) -> Value {
        let mut body = serde_json::Map::new();
        if let Some(obj) = props.as_object() {
            for (k, v) in obj {
                if k != "Name" && !v.is_null() {
                    body.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(body)
    }

    /// `AWS::Scheduler::Schedule`. `Ref` returns the schedule name.
    pub(super) fn create_scheduler_schedule(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let generated = self.physical_name(resource);
        let name = props
            .get("Name")
            .and_then(|v| v.as_str())
            .unwrap_or(&generated)
            .to_string();
        let out = self.scheduler_dispatch(
            "CreateSchedule",
            &name,
            Self::schedule_body(props),
            HashMap::new(),
        )?;
        let arn = out["ScheduleArn"].as_str().unwrap_or_default().to_string();
        Ok(ProvisionResult::new(name).with("Arn", arn))
    }

    /// In place when the group is unchanged (UpdateSchedule replaces the
    /// whole definition, so dropped properties reset); a new group or name
    /// replaces the schedule, as CloudFormation does.
    pub(super) fn update_scheduler_schedule(
        &self,
        existing: &StackResource,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let old_group = existing
            .attributes
            .get("Arn")
            .and_then(|a| group_of_schedule_arn(a))
            .unwrap_or("default")
            .to_string();
        let new_group = props
            .get("GroupName")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        let new_name = props.get("Name").and_then(|v| v.as_str());
        if new_group != old_group || new_name.is_some_and(|n| n != existing.physical_id) {
            self.delete_scheduler_schedule(existing)?;
            return self.create_scheduler_schedule(resource);
        }
        let out = self.scheduler_dispatch(
            "UpdateSchedule",
            &existing.physical_id,
            Self::schedule_body(props),
            HashMap::new(),
        )?;
        let arn = out["ScheduleArn"].as_str().unwrap_or_default().to_string();
        Ok(ProvisionResult::new(existing.physical_id.clone()).with("Arn", arn))
    }

    pub(super) fn delete_scheduler_schedule(&self, resource: &StackResource) -> Result<(), String> {
        let group = resource
            .attributes
            .get("Arn")
            .and_then(|a| group_of_schedule_arn(a))
            .unwrap_or("default");
        let mut query = HashMap::new();
        query.insert("groupName".to_string(), group.to_string());
        // Already gone (deleted out of band, or with its group) is not a
        // stack failure.
        let _ = self.scheduler_dispatch(
            "DeleteSchedule",
            &resource.physical_id,
            Value::Null,
            query,
        );
        Ok(())
    }

    /// `AWS::Scheduler::ScheduleGroup`. `Ref` returns the group name.
    pub(super) fn create_scheduler_schedule_group(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let generated = self.physical_name(resource);
        let name = props
            .get("Name")
            .and_then(|v| v.as_str())
            .unwrap_or(&generated)
            .to_string();
        let mut body = serde_json::Map::new();
        if let Some(tags) = props.get("Tags").filter(|v| v.is_array()) {
            body.insert("Tags".to_string(), tags.clone());
        }
        let out = self.scheduler_dispatch(
            "CreateScheduleGroup",
            &name,
            Value::Object(body),
            HashMap::new(),
        )?;
        let arn = out["ScheduleGroupArn"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(ProvisionResult::new(name)
            .with("Arn", arn)
            .with("State", "ACTIVE"))
    }

    /// Only `Tags` is mutable; the group (and the schedules in it) stays.
    pub(super) fn update_scheduler_schedule_group(
        &self,
        existing: &StackResource,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let tags: BTreeMap<String, String> = resource
            .properties
            .get("Tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some((
                            t.get("Key")?.as_str()?.to_string(),
                            t.get("Value")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        SchedulerService::new(self.scheduler_state.clone())
            .replace_schedule_group_tags(&self.account_id, &existing.physical_id, tags)
            .map_err(|e| format!("{}: {}", e.code(), e.message()))?;
        let mut result = ProvisionResult::new(existing.physical_id.clone());
        for (k, v) in &existing.attributes {
            result = result.with(k, v.clone());
        }
        Ok(result)
    }

    pub(super) fn delete_scheduler_schedule_group(&self, physical_id: &str) -> Result<(), String> {
        let _ = self.scheduler_dispatch(
            "DeleteScheduleGroup",
            physical_id,
            Value::Null,
            HashMap::new(),
        );
        Ok(())
    }
}
