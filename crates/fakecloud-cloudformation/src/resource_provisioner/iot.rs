//! `AWS::IoT::TopicRule`: stores the rule in the IoT control-plane state
//! exactly as `CreateTopicRule` / `ReplaceTopicRule` do (the camelCase
//! `topicRulePayload` plus `ruleName` / `ruleArn` / `createdAt`), so
//! `GetTopicRule` / `ListTopicRules` read a stack's rule back like one created
//! through the API.

use super::*;

impl ResourceProvisioner {
    /// The record `CreateTopicRule` stores for `name`, from the template's
    /// `TopicRulePayload`.
    fn iot_topic_rule_record(
        &self,
        name: &str,
        props: &serde_json::Value,
        created_at: Option<serde_json::Value>,
    ) -> Result<(String, serde_json::Value), String> {
        let payload = props
            .get("TopicRulePayload")
            .filter(|v| v.is_object())
            .ok_or("TopicRulePayload is required")?;
        if payload
            .get("Sql")
            .and_then(|v| v.as_str())
            .is_none_or(str::is_empty)
        {
            return Err("TopicRulePayload.Sql is required".to_string());
        }
        if !payload.get("Actions").is_some_and(|v| v.is_array()) {
            return Err("TopicRulePayload.Actions is required".to_string());
        }
        let mut record = cfn_props_to_camel(payload, &[]);
        let arn =
            fakecloud_iot::service::resource_arn(&self.region, &self.account_id, "rules", name);
        let obj = record
            .as_object_mut()
            .ok_or("TopicRulePayload must be an object")?;
        obj.insert("ruleName".into(), serde_json::json!(name));
        obj.insert("ruleArn".into(), serde_json::json!(arn));
        obj.entry("ruleDisabled")
            .or_insert(serde_json::Value::Bool(false));
        obj.insert(
            "createdAt".into(),
            created_at.unwrap_or_else(|| {
                serde_json::json!(Utc::now().timestamp_millis() as f64 / 1000.0)
            }),
        );
        Ok((arn, record))
    }

    fn iot_topic_rule_tags(props: &serde_json::Value) -> BTreeMap<String, String> {
        props
            .get("Tags")
            .and_then(|v| v.as_array())
            .map(|tags| {
                tags.iter()
                    .filter_map(|t| {
                        Some((
                            t.get("Key")?.as_str()?.to_string(),
                            t.get("Value")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn create_iot_topic_rule(
        &self,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let props = &resource.properties;
        let name = props
            .get("RuleName")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.physical_name(resource));
        let (arn, record) = self.iot_topic_rule_record(&name, props, None)?;

        let mut accounts = self.iot_state.write();
        let data = accounts.get_or_create(&self.account_id);
        if data.get_resource("rules", &name).is_some() {
            return Err(resource_already_exists("AWS::IoT::TopicRule", &name));
        }
        data.put_resource("rules", &name, record);
        data.set_tags(&arn, Self::iot_topic_rule_tags(props));
        Ok(ProvisionResult::new(name).with("Arn", arn))
    }

    /// A payload change replaces the rule's definition in place, as
    /// `ReplaceTopicRule` does; a new `RuleName` is a replacement, which the
    /// engine handles.
    pub(super) fn update_iot_topic_rule(
        &self,
        existing: &StackResource,
        resource: &ResourceDefinition,
    ) -> Result<ProvisionResult, String> {
        let name = &existing.physical_id;
        let props = &resource.properties;
        let created_at = {
            let accounts = self.iot_state.read();
            accounts
                .get(&self.account_id)
                .and_then(|d| d.get_resource("rules", name))
                .and_then(|r| r.get("createdAt").cloned())
        };
        let (arn, record) = self.iot_topic_rule_record(name, props, created_at)?;
        let mut accounts = self.iot_state.write();
        let data = accounts.get_or_create(&self.account_id);
        data.put_resource("rules", name, record);
        data.set_tags(&arn, Self::iot_topic_rule_tags(props));
        Ok(ProvisionResult::new(name.clone()).with("Arn", arn))
    }

    pub(super) fn delete_iot_topic_rule(&self, physical_id: &str) -> Result<(), String> {
        let arn = fakecloud_iot::service::resource_arn(
            &self.region,
            &self.account_id,
            "rules",
            physical_id,
        );
        let mut accounts = self.iot_state.write();
        let data = accounts.get_or_create(&self.account_id);
        data.remove_resource("rules", physical_id);
        data.remove_tags(&arn);
        Ok(())
    }
}
