//! `for_each` concerns from template.rs (audit-2026-05-19).

use super::*;

/// Expand `Fn::ForEach::<UniqueLoopName>` macros in `value` recursively.
///
/// Syntax (from the AWS docs / sample):
/// ```text
/// "Fn::ForEach::TopicLoop": [
///   "LoopVar",
///   ["a", "b", "c"],
///   { "${LoopVar}Topic": { "Type": "AWS::SNS::Topic", ... } }
/// ]
/// ```
/// becomes three siblings (`aTopic`, `bTopic`, `cTopic`) in the parent
/// object. `${LoopVar}` substitutes inside both keys and values, so the
/// emitted body can reference the iteration value the same way `Fn::Sub`
/// does.
///
/// Macros nest: an outer ForEach's bindings flow into inner ForEach
/// bodies via `bindings`, so `${OuterVar}` resolves inside an inner
/// loop's body. Each call resolves its own loop variable's iterations
/// before recursing into the emitted entries.
pub(crate) fn expand_for_each(
    value: &Value,
    bindings: &BTreeMap<String, String>,
    parameters: &BTreeMap<String, String>,
) -> Result<Value, String> {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if let Some(loop_name) = k.strip_prefix("Fn::ForEach::") {
                    let arr = v.as_array().ok_or_else(|| {
                        format!("Fn::ForEach::{loop_name} requires an array argument")
                    })?;
                    if arr.len() != 3 {
                        return Err(format!(
                            "Fn::ForEach::{loop_name} requires 3 arguments (loopVar, list, template), got {}",
                            arr.len()
                        ));
                    }
                    let loop_var = arr[0].as_str().ok_or_else(|| {
                        format!("Fn::ForEach::{loop_name} loop variable must be a string")
                    })?;
                    // The items list may be a literal array OR a `Ref`
                    // to a CommaDelimitedList parameter (AWS-supported).
                    // Resolve the latter against `parameters` by
                    // splitting on `,` so the loop iterates the same
                    // values the template author wrote.
                    let items_owned: Vec<Value> =
                        resolve_for_each_items(&arr[1], parameters).ok_or_else(|| {
                            format!(
                                "Fn::ForEach::{loop_name} second argument must be an array or a Ref to a CommaDelimitedList parameter"
                            )
                        })?;
                    let body = arr[2].as_object().ok_or_else(|| {
                        format!("Fn::ForEach::{loop_name} third argument must be an object")
                    })?;
                    for item in &items_owned {
                        let item_str = match item {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let mut next = bindings.clone();
                        next.insert(loop_var.to_string(), item_str.clone());
                        // Substitute loop vars across the whole body
                        // first, then recurse via `expand_for_each` so
                        // any nested `Fn::ForEach::*` keys land inline
                        // as sibling entries of `out` (instead of
                        // wrapping them under the unresolved macro key).
                        let body_value = Value::Object(body.clone());
                        let substituted = substitute_loop_vars_in_value(&body_value, &next);
                        let expanded = expand_for_each(&substituted, &next, parameters)?;
                        if let Value::Object(emitted) = expanded {
                            for (ek, ev) in emitted {
                                out.insert(ek, ev);
                            }
                        }
                    }
                    continue;
                }
                out.insert(k.clone(), expand_for_each(v, bindings, parameters)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(expand_for_each(v, bindings, parameters)?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

/// Expand AWS::Serverless-2016-10-31 SAM resources into native
/// CloudFormation resources so the provisioner can handle them. A template
/// the SAM translator would reject (an unsupported event type, an event
/// referencing something it cannot) fails the transform.
pub(super) fn expand_sam(value: &Value) -> Result<Value, String> {
    let transform = value.get("Transform");
    let has_sam = match transform {
        Some(Value::String(s)) => s == "AWS::Serverless-2016-10-31",
        Some(Value::Array(arr)) => arr
            .iter()
            .any(|v| v.as_str() == Some("AWS::Serverless-2016-10-31")),
        _ => false,
    };
    if !has_sam {
        return Ok(value.clone());
    }

    // SAM `Globals` supply default properties for every resource of a given
    // type; per-resource Properties override them per key. Real `sam`/
    // samtranslator applies these during the transform, so without merging them
    // here a function that sets Handler/Runtime only in `Globals.Function`
    // deploys with the bare Lambda defaults (index.handler / python3.12) and
    // fails every invoke with `Runtime.HandlerNotFound`.
    let global = |section: &str| {
        value
            .get("Globals")
            .and_then(|g| g.get(section))
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    };
    let global_function = global("Function");
    let global_api = global("Api");
    let global_http_api = global("HttpApi");
    let global_simple_table = global("SimpleTable");
    let global_state_machine = global("StateMachine");

    let mut value = value.clone();
    let Some(resources) = value.get_mut("Resources") else {
        return Ok(value);
    };
    let Some(resources_map) = resources.as_object_mut() else {
        return Ok(value);
    };

    // Immutable snapshot of the original resource set, used to resolve a
    // Connector's Source/Destination types (a Connector references other
    // logical resources by id) while the main loop iterates.
    let resources_map_snapshot = resources_map.clone();

    let mut new_resources = serde_json::Map::new();
    // Api/HttpApi routes collected from every function's Events, plus the
    // edits events make to other resources, applied after the loop.
    let mut ctx = super::sam_events::SamContext {
        resources: resources_map_snapshot.clone(),
        ..Default::default()
    };
    // Explicit Serverless::Api / HttpApi resources, expanded once every
    // function's routes are known.
    let mut rest_apis: Vec<super::sam_api::ApiDef> = Vec::new();
    let mut http_apis: Vec<super::sam_api::ApiDef> = Vec::new();
    for (logical_id, resource) in resources_map.iter() {
        let Some(resource_obj) = resource.as_object() else {
            new_resources.insert(logical_id.clone(), resource.clone());
            continue;
        };
        let Some(ty) = resource_obj.get("Type").and_then(|v| v.as_str()) else {
            new_resources.insert(logical_id.clone(), resource.clone());
            continue;
        };
        let properties = resource_obj
            .get("Properties")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        match ty {
            "AWS::Serverless::Function" => {
                let mut lambda_props = merge_global_properties(&global_function, &properties);
                // Map CodeUri / InlineCode to Code
                if let Some(code_uri) = lambda_props.get("CodeUri").cloned() {
                    lambda_props.remove("CodeUri");
                    let code = if let Some(s) = code_uri.as_str() {
                        if let Some(stripped) = s.strip_prefix("s3://") {
                            let parts: Vec<&str> = stripped.splitn(2, '/').collect();
                            if parts.len() == 2 {
                                json!({"S3Bucket": parts[0], "S3Key": parts[1]})
                            } else {
                                json!({"S3Bucket": "sam", "S3Key": s})
                            }
                        } else {
                            json!({"S3Bucket": "sam", "S3Key": s})
                        }
                    } else {
                        code_uri
                    };
                    lambda_props.insert("Code".to_string(), code);
                } else if let Some(inline) = lambda_props.get("InlineCode").cloned() {
                    lambda_props.remove("InlineCode");
                    lambda_props.insert("Code".to_string(), json!({"ZipFile": inline}));
                }
                // Expand the function's `Policies` (-> implicit execution role)
                // and non-API `Events` (-> Events::Rule / EventSourceMapping /
                // SNS::Subscription + Lambda::Permission). Mutates lambda_props
                // (sets Role, removes Policies/Events) and returns the extra
                // native resources to add. Without this SAM functions deploy
                // with no role and no triggers.
                let extras = super::sam_events::expand_function_extras(
                    logical_id,
                    &mut lambda_props,
                    &mut ctx,
                )?;

                let mut lambda_resource = serde_json::Map::new();
                lambda_resource.insert("Type".to_string(), json!("AWS::Lambda::Function"));
                lambda_resource.insert("Properties".to_string(), Value::Object(lambda_props));
                for (k, v) in resource_obj {
                    if k != "Type" && k != "Properties" {
                        lambda_resource.insert(k.clone(), v.clone());
                    }
                }
                new_resources.insert(logical_id.clone(), Value::Object(lambda_resource));
                for (extra_id, extra) in extras {
                    new_resources.entry(extra_id).or_insert(extra);
                }
            }
            "AWS::Serverless::Api" | "AWS::Serverless::HttpApi" => {
                let globals = if ty == "AWS::Serverless::Api" {
                    &global_api
                } else {
                    &global_http_api
                };
                let def = super::sam_api::ApiDef {
                    logical_id: logical_id.clone(),
                    props: merge_global_properties(globals, &properties),
                    attributes: resource_obj
                        .iter()
                        .filter(|(k, _)| *k != "Type" && *k != "Properties")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                };
                if ty == "AWS::Serverless::Api" {
                    rest_apis.push(def);
                } else {
                    http_apis.push(def);
                }
            }
            "AWS::Serverless::SimpleTable" => {
                let mut table_props = merge_global_properties(&global_simple_table, &properties);
                if let Some(pk) = table_props.get("PrimaryKey") {
                    if let Some(pk_obj) = pk.as_object() {
                        let name = pk_obj.get("Name").cloned().unwrap_or_else(|| json!("id"));
                        let ty = match pk_obj.get("Type").and_then(|v| v.as_str()) {
                            Some("String") => json!("S"),
                            Some("Number") => json!("N"),
                            Some("Binary") => json!("B"),
                            Some(other) => json!(other),
                            None => json!("S"),
                        };
                        table_props.remove("PrimaryKey");
                        table_props.insert(
                            "KeySchema".to_string(),
                            json!([{"AttributeName": name.clone(), "KeyType": "HASH"}]),
                        );
                        table_props.insert(
                            "AttributeDefinitions".to_string(),
                            json!([{"AttributeName": name, "AttributeType": ty}]),
                        );
                    }
                }
                if !table_props.contains_key("BillingMode") {
                    table_props.insert("BillingMode".to_string(), json!("PAY_PER_REQUEST"));
                }
                let mut table_resource = serde_json::Map::new();
                table_resource.insert("Type".to_string(), json!("AWS::DynamoDB::Table"));
                table_resource.insert("Properties".to_string(), Value::Object(table_props));
                for (k, v) in resource_obj {
                    if k != "Type" && k != "Properties" {
                        table_resource.insert(k.clone(), v.clone());
                    }
                }
                new_resources.insert(logical_id.clone(), Value::Object(table_resource));
            }
            "AWS::Serverless::LayerVersion" => {
                let mut layer_props = if let Some(p) = properties.as_object() {
                    p.clone()
                } else {
                    serde_json::Map::new()
                };
                if let Some(uri) = layer_props.get("ContentUri").cloned() {
                    layer_props.remove("ContentUri");
                    let content = if let Some(s) = uri.as_str() {
                        if let Some(stripped) = s.strip_prefix("s3://") {
                            let parts: Vec<&str> = stripped.splitn(2, '/').collect();
                            if parts.len() == 2 {
                                json!({"S3Bucket": parts[0], "S3Key": parts[1]})
                            } else {
                                json!({"S3Bucket": "sam", "S3Key": s})
                            }
                        } else {
                            json!({"S3Bucket": "sam", "S3Key": s})
                        }
                    } else {
                        uri
                    };
                    layer_props.insert("Content".to_string(), content);
                }
                let mut layer_resource = serde_json::Map::new();
                layer_resource.insert("Type".to_string(), json!("AWS::Lambda::LayerVersion"));
                layer_resource.insert("Properties".to_string(), Value::Object(layer_props));
                for (k, v) in resource_obj {
                    if k != "Type" && k != "Properties" {
                        layer_resource.insert(k.clone(), v.clone());
                    }
                }
                new_resources.insert(logical_id.clone(), Value::Object(layer_resource));
            }
            "AWS::Serverless::StateMachine" => {
                let mut sfn_props = merge_global_properties(&global_state_machine, &properties);
                // `Definition` (inline ASL object) passes through unchanged —
                // the native provisioner's resolve_sfn_definition reads it
                // directly. `DefinitionSubstitutions` likewise passes through.
                // Map `DefinitionUri` onto `DefinitionS3Location`, reusing the
                // same `s3://bucket/key` parsing the Function arm uses for
                // CodeUri. SAM also allows the object form
                // `{Bucket, Key, Version}`, which maps over verbatim.
                if let Some(uri) = sfn_props.get("DefinitionUri").cloned() {
                    sfn_props.remove("DefinitionUri");
                    let location = if let Some(s) = uri.as_str() {
                        if let Some(stripped) = s.strip_prefix("s3://") {
                            let parts: Vec<&str> = stripped.splitn(2, '/').collect();
                            if parts.len() == 2 {
                                json!({"Bucket": parts[0], "Key": parts[1]})
                            } else {
                                json!({"Bucket": "sam", "Key": s})
                            }
                        } else {
                            json!({"Bucket": "sam", "Key": s})
                        }
                    } else {
                        // Already an object form: {Bucket, Key, Version}.
                        uri
                    };
                    sfn_props.insert("DefinitionS3Location".to_string(), location);
                }
                // `Role` (ARN) -> `RoleArn`.
                if let Some(role) = sfn_props.remove("Role") {
                    sfn_props.insert("RoleArn".to_string(), role);
                }
                // `Name` -> `StateMachineName`.
                if let Some(name) = sfn_props.remove("Name") {
                    sfn_props.insert("StateMachineName".to_string(), name);
                }
                // SAM `Type` (STANDARD|EXPRESS) -> native `StateMachineType`.
                if let Some(machine_type) = sfn_props.remove("Type") {
                    sfn_props.insert("StateMachineType".to_string(), machine_type);
                }
                // Expand `Events` (Schedule/ScheduleV2/EventBridgeRule/
                // CloudWatchEvent/Api/HttpApi) into the corresponding native
                // trigger resources targeting the state machine, mirroring the
                // function-events expansion. Without this a scheduled/event-
                // driven SAM state machine deployed with no trigger at all.
                let sfn_extras = sfn_props
                    .remove("Events")
                    .and_then(|e| e.as_object().cloned())
                    .map(|events| {
                        super::sam_events::expand_state_machine_events(logical_id, &events)
                    })
                    .transpose()?
                    .unwrap_or_default();

                let mut sfn_resource = serde_json::Map::new();
                sfn_resource.insert(
                    "Type".to_string(),
                    json!("AWS::StepFunctions::StateMachine"),
                );
                sfn_resource.insert("Properties".to_string(), Value::Object(sfn_props));
                for (k, v) in resource_obj {
                    if k != "Type" && k != "Properties" {
                        sfn_resource.insert(k.clone(), v.clone());
                    }
                }
                new_resources.insert(logical_id.clone(), Value::Object(sfn_resource));
                for (extra_id, extra) in sfn_extras {
                    new_resources.entry(extra_id).or_insert(extra);
                }
            }
            "AWS::Serverless::Connector" => {
                // A Connector is pure IAM sugar: it grants the Source's role the
                // requested Read/Write actions on the Destination. Expand it into
                // the equivalent IAM policy resource(s) so the deploy actually
                // wires up access instead of recording a phantom resource with no
                // backing. `resources_map` is consulted to resolve the
                // Destination's (SAM or native) type when the connector doesn't
                // state it inline.
                let connector_extras = super::sam_events::expand_connector(
                    logical_id,
                    &properties,
                    &resources_map_snapshot,
                );
                for (extra_id, extra) in connector_extras {
                    new_resources.entry(extra_id).or_insert(extra);
                }
            }
            "AWS::Serverless::Application" => {
                // A nested SAR/template application. Expand it into a native
                // `AWS::CloudFormation::Stack` (which has a real provisioner +
                // nested-stack persistence) pointing at the referenced template,
                // carrying the `Parameters` through, instead of recording a
                // no-backing phantom.
                let stack_resource =
                    super::sam_events::expand_application(&properties, resource_obj);
                new_resources.insert(logical_id.clone(), stack_resource);
            }
            _ => {
                new_resources.insert(logical_id.clone(), resource.clone());
            }
        }
    }

    // The implicit APIs SAM creates for events without an explicit
    // RestApiId / ApiId: `ServerlessRestApi` (stage `Prod`) and
    // `ServerlessHttpApi` (stage `$default`), configured by Globals.
    let implicit = |id: &str, globals: &serde_json::Map<String, Value>| super::sam_api::ApiDef {
        logical_id: id.to_string(),
        props: globals.clone(),
        attributes: serde_json::Map::new(),
    };
    let routes_for = |api_id: &str, http: bool| -> Vec<&super::sam_events::ApiRoute> {
        ctx.api_routes
            .iter()
            .filter(|r| r.api_id == api_id && r.http_api == http)
            .collect()
    };
    let mut api_resources: Vec<(String, Value)> = Vec::new();
    let mut api_refs: Vec<(String, String)> = Vec::new();
    if !routes_for("ServerlessRestApi", false).is_empty()
        && !rest_apis
            .iter()
            .any(|d| d.logical_id == "ServerlessRestApi")
    {
        let mut def = implicit("ServerlessRestApi", &global_api);
        def.props.remove("StageName");
        let (res, refs) = super::sam_api::build_rest_api(
            &def,
            &routes_for("ServerlessRestApi", false),
            Some("Prod"),
        )?;
        api_resources.extend(res);
        api_refs.extend(refs);
    }
    if !routes_for("ServerlessHttpApi", true).is_empty()
        && !http_apis
            .iter()
            .any(|d| d.logical_id == "ServerlessHttpApi")
    {
        let def = implicit("ServerlessHttpApi", &global_http_api);
        let (res, refs) =
            super::sam_api::build_http_api(&def, &routes_for("ServerlessHttpApi", true))?;
        api_resources.extend(res);
        api_refs.extend(refs);
    }
    for def in &rest_apis {
        let (res, refs) =
            super::sam_api::build_rest_api(def, &routes_for(&def.logical_id, false), None)?;
        api_resources.extend(res);
        api_refs.extend(refs);
    }
    for def in &http_apis {
        let (res, refs) = super::sam_api::build_http_api(def, &routes_for(&def.logical_id, true))?;
        api_resources.extend(res);
        api_refs.extend(refs);
    }
    for (id, res) in api_resources {
        new_resources.entry(id).or_insert(res);
    }
    super::sam_events::apply_cross_resource_edits(&ctx, &mut new_resources)?;

    resources_map.clear();
    for (k, v) in new_resources {
        resources_map.insert(k, v);
    }

    // `Ref: MyFunction.Alias` / `MyApi.Stage` and friends name generated
    // resources; point them at their logical ids everywhere they appear.
    let mut aliases = ctx.ref_aliases.clone();
    aliases.extend(api_refs);
    if let Some(obj) = value.as_object_mut() {
        for section in ["Resources", "Outputs", "Conditions"] {
            if let Some(v) = obj.get_mut(section) {
                super::sam_events::rewrite_sam_refs(v, &aliases);
            }
        }
    }
    Ok(value)
}

/// Merge SAM `Globals.<Section>` defaults under a resource's own `Properties`,
/// following AWS SAM's combination rules:
/// - **Scalars** (`Handler`, `Runtime`, `Timeout`, ...): the resource value
///   overrides the global.
/// - **Maps** (`Environment.Variables`, `Tags`): deep-merged, so global entries
///   survive unless the resource sets the same sub-key.
/// - **Additive lists** (`Layers`, `Policies`): the global list is prepended to
///   the resource list rather than replaced.
fn merge_global_properties(
    globals: &serde_json::Map<String, Value>,
    properties: &Value,
) -> serde_json::Map<String, Value> {
    let mut merged = globals.clone();
    if let Some(p) = properties.as_object() {
        for (k, v) in p {
            match merged.remove(k) {
                Some(global_v) => {
                    merged.insert(k.clone(), merge_global_value(k, global_v, v.clone()));
                }
                None => {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
    }
    merged
}

/// SAM list-typed Globals that are combined (global ++ resource) rather than
/// overridden. Per the SAM Globals spec these are additive.
const ADDITIVE_GLOBAL_LISTS: &[&str] = &["Layers", "Policies"];

/// Combine a single global value with its resource counterpart per SAM rules.
/// `key` is the property name, used to decide whether a list is additive.
fn merge_global_value(key: &str, global_v: Value, resource_v: Value) -> Value {
    match (global_v, resource_v) {
        // Maps deep-merge: walk shared keys recursively, resource wins on leaves.
        (Value::Object(global_map), Value::Object(resource_map)) => {
            let mut out = global_map;
            for (k, v) in resource_map {
                match out.remove(&k) {
                    Some(existing) => {
                        out.insert(k.clone(), merge_global_value(&k, existing, v));
                    }
                    None => {
                        out.insert(k, v);
                    }
                }
            }
            Value::Object(out)
        }
        // Additive lists (Layers, Policies) combine global first, then resource.
        (Value::Array(mut global_list), Value::Array(resource_list))
            if ADDITIVE_GLOBAL_LISTS.contains(&key) =>
        {
            global_list.extend(resource_list);
            Value::Array(global_list)
        }
        // Everything else: the resource value wins.
        (_, resource_v) => resource_v,
    }
}

/// Resolve the `items` argument of an `Fn::ForEach` macro. Accepts:
/// - A literal JSON array — returned as-is.
/// - `{ "Ref": "<name>" }` against a parameter holding either a comma
///   delimited list (`CommaDelimitedList` / `List<*>`) or a single
///   value. Splits on `,` and trims whitespace so parameters set as
///   `"a, b, c"` iterate cleanly.
///
/// Returns `None` for any other shape (e.g. an object that isn't a
/// `Ref`, or a `Ref` to an undefined parameter), letting the caller
/// surface a precise error.
pub(super) fn resolve_for_each_items(
    value: &Value,
    parameters: &BTreeMap<String, String>,
) -> Option<Vec<Value>> {
    if let Some(arr) = value.as_array() {
        return Some(arr.clone());
    }
    if let Some(map) = value.as_object() {
        if let Some(name) = map.get("Ref").and_then(|v| v.as_str()) {
            let raw = parameters.get(name)?;
            return Some(
                raw.split(',')
                    .map(|p| Value::String(p.trim().to_string()))
                    .collect(),
            );
        }
    }
    None
}

/// Substitute every `${var}` and `&{var}` token in a string against
/// `bindings`. Both forms are AWS-supported for `Fn::ForEach` loop
/// variables — `&{}` exists so identifiers with non-alphanumeric
/// characters can interpolate into resource logical IDs without
/// colliding with Fn::Sub's `${}` syntax. Unknown vars stay verbatim
/// so non-loop substitutions (Fn::Sub, resource physical IDs) handle
/// them later.
pub(super) fn substitute_loop_vars(s: &str, bindings: &BTreeMap<String, String>) -> String {
    let mut result = s.to_string();
    for (k, v) in bindings {
        result = result.replace(&format!("${{{k}}}"), v);
        result = result.replace(&format!("&{{{k}}}"), v);
    }
    result
}

/// Walk `value` and apply `substitute_loop_vars` to every string leaf.
/// Object keys are also rewritten so resource logical IDs and property
/// names parameterized by the loop variable land correctly.
pub(super) fn substitute_loop_vars_in_value(
    value: &Value,
    bindings: &BTreeMap<String, String>,
) -> Value {
    match value {
        Value::String(s) => Value::String(substitute_loop_vars(s, bindings)),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let new_key = substitute_loop_vars(k, bindings);
                out.insert(new_key, substitute_loop_vars_in_value(v, bindings));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| substitute_loop_vars_in_value(v, bindings))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Gap #4: Handler/Runtime set only in `Globals.Function` must land on the
    // expanded Lambda; otherwise the function deploys with the bare defaults
    // (index.handler / python3.12) and every invoke raises Runtime.HandlerNotFound.
    #[test]
    fn expand_sam_applies_function_globals() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Globals": {
                "Function": {
                    "Handler": "index.lambda_handler",
                    "Runtime": "python3.13",
                    "Timeout": 300,
                    "MemorySize": 256
                }
            },
            "Resources": {
                "Dispatcher": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": {
                        "FunctionName": "workflow_dispatcher_v2",
                        "InlineCode": "def lambda_handler(e, c): return e"
                    }
                }
            }
        });

        let props = &expand_sam(&template).unwrap()["Resources"]["Dispatcher"]["Properties"];
        assert_eq!(props["Handler"], json!("index.lambda_handler"));
        assert_eq!(props["Runtime"], json!("python3.13"));
        assert_eq!(props["Timeout"], json!(300));
        assert_eq!(props["MemorySize"], json!(256));
        // Per-function properties are preserved alongside the merged globals.
        assert_eq!(props["FunctionName"], json!("workflow_dispatcher_v2"));
    }

    // Per-function Properties override Globals per key.
    #[test]
    fn expand_sam_function_overrides_globals() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Globals": {"Function": {"Handler": "index.lambda_handler", "Runtime": "python3.13"}},
            "Resources": {
                "F": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": {"Handler": "app.main", "InlineCode": "x"}
                }
            }
        });

        let props = &expand_sam(&template).unwrap()["Resources"]["F"]["Properties"];
        assert_eq!(
            props["Handler"],
            json!("app.main"),
            "per-function Handler wins"
        );
        assert_eq!(
            props["Runtime"],
            json!("python3.13"),
            "global Runtime still applies"
        );
    }

    // Map-typed Globals (Environment.Variables, Tags) deep-merge instead of
    // being wholesale-replaced by a resource that sets the same property.
    #[test]
    fn expand_sam_function_globals_deep_merge_maps() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Globals": {
                "Function": {
                    "Environment": {"Variables": {"STAGE": "prod", "REGION": "us-east-1"}},
                    "Tags": {"team": "core", "env": "prod"}
                }
            },
            "Resources": {
                "F": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": {
                        "Handler": "app.main",
                        "Runtime": "python3.13",
                        "Environment": {"Variables": {"REGION": "eu-west-1", "DEBUG": "1"}},
                        "Tags": {"env": "staging"}
                    }
                }
            }
        });

        let props = &expand_sam(&template).unwrap()["Resources"]["F"]["Properties"];
        // Global var survives, resource var overrides shared key, resource adds new.
        assert_eq!(props["Environment"]["Variables"]["STAGE"], json!("prod"));
        assert_eq!(
            props["Environment"]["Variables"]["REGION"],
            json!("eu-west-1")
        );
        assert_eq!(props["Environment"]["Variables"]["DEBUG"], json!("1"));
        // Tags merge the same way.
        assert_eq!(props["Tags"]["team"], json!("core"));
        assert_eq!(props["Tags"]["env"], json!("staging"));
    }

    // List-typed additive Globals (Layers, Policies) combine global ++ resource.
    #[test]
    fn expand_sam_function_globals_additive_lists() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Globals": {
                "Function": {
                    "Layers": ["arn:aws:lambda:::layer:global:1"],
                    "Policies": ["AWSLambdaBasicExecutionRole"]
                }
            },
            "Resources": {
                "F": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": {
                        "Handler": "app.main",
                        "Runtime": "python3.13",
                        "Layers": ["arn:aws:lambda:::layer:local:2"],
                        "Policies": ["AmazonS3ReadOnlyAccess"]
                    }
                }
            }
        });

        let expanded = expand_sam(&template).unwrap();
        let props = &expanded["Resources"]["F"]["Properties"];
        assert_eq!(
            props["Layers"],
            json!([
                "arn:aws:lambda:::layer:global:1",
                "arn:aws:lambda:::layer:local:2"
            ])
        );
        // `Policies` are now additively merged (Globals ++ resource) and then
        // consumed into the synthesized execution role's ManagedPolicyArns,
        // rather than left on the function Properties.
        assert!(props.get("Policies").is_none());
        let role_arns = &expanded["Resources"]["FRole"]["Properties"]["ManagedPolicyArns"];
        let arns = role_arns.as_array().unwrap();
        assert!(arns
            .iter()
            .any(|a| a["Fn::Sub"]
                == "arn:${AWS::Partition}:iam::aws:policy/AWSLambdaBasicExecutionRole"));
        assert!(arns.iter().any(
            |a| a["Fn::Sub"] == "arn:${AWS::Partition}:iam::aws:policy/AmazonS3ReadOnlyAccess"
        ));
    }

    // Globals on the non-Function sections (Api/HttpApi/SimpleTable/StateMachine)
    // are applied too.
    #[test]
    fn expand_sam_applies_non_function_globals() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Globals": {
                "Api": {"Cors": "'*'"},
                "SimpleTable": {"SSESpecification": {"SSEEnabled": true}}
            },
            "Resources": {
                "Gw": {"Type": "AWS::Serverless::Api", "Properties": {"StageName": "prod"}},
                "Tbl": {"Type": "AWS::Serverless::SimpleTable", "Properties": {}}
            }
        });

        let expanded = expand_sam(&template).unwrap();
        // The global Cors reaches the API: with no paths there is nothing to
        // preflight, so it is consumed without adding OPTIONS methods.
        assert!(expanded["Resources"]["Gw"]["Properties"]
            .get("Cors")
            .is_none());
        assert_eq!(
            expanded["Resources"]["GwprodStage"]["Properties"]["StageName"],
            json!("prod")
        );
        assert_eq!(
            expanded["Resources"]["Tbl"]["Properties"]["SSESpecification"]["SSEEnabled"],
            json!(true)
        );
    }

    #[test]
    fn expand_sam_statemachine_inline_definition() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "MySM": {
                    "Type": "AWS::Serverless::StateMachine",
                    "DependsOn": "SomeOtherResource",
                    "Properties": {
                        "Definition": {
                            "StartAt": "Done",
                            "States": {"Done": {"Type": "Succeed"}}
                        },
                        "DefinitionSubstitutions": {"fn": "my-fn"},
                        "Role": "arn:aws:iam::123456789012:role/sfn-role",
                        "Name": "my-state-machine",
                        "Type": "EXPRESS"
                    }
                }
            }
        });

        let expanded = expand_sam(&template).unwrap();
        let resource = &expanded["Resources"]["MySM"];

        assert_eq!(resource["Type"], json!("AWS::StepFunctions::StateMachine"));

        let props = &resource["Properties"];
        assert_eq!(
            props["RoleArn"],
            json!("arn:aws:iam::123456789012:role/sfn-role")
        );
        assert_eq!(props["StateMachineName"], json!("my-state-machine"));
        assert_eq!(props["StateMachineType"], json!("EXPRESS"));
        // Inline definition carries over unchanged for the provisioner to read.
        assert_eq!(
            props["Definition"],
            json!({"StartAt": "Done", "States": {"Done": {"Type": "Succeed"}}})
        );
        // DefinitionSubstitutions passes through.
        assert_eq!(props["DefinitionSubstitutions"], json!({"fn": "my-fn"}));
        // SAM-only keys are gone.
        assert!(props.get("Role").is_none());
        assert!(props.get("Name").is_none());
        // Resource-level metadata is preserved.
        assert_eq!(resource["DependsOn"], json!("SomeOtherResource"));
    }

    #[test]
    fn expand_sam_statemachine_definition_uri() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "MySM": {
                    "Type": "AWS::Serverless::StateMachine",
                    "Properties": {
                        "DefinitionUri": "s3://my-bucket/path/to/def.asl.json",
                        "Role": "arn:aws:iam::123456789012:role/sfn-role"
                    }
                }
            }
        });

        let expanded = expand_sam(&template).unwrap();
        let props = &expanded["Resources"]["MySM"]["Properties"];

        assert_eq!(
            props["DefinitionS3Location"],
            json!({"Bucket": "my-bucket", "Key": "path/to/def.asl.json"})
        );
        assert!(props.get("DefinitionUri").is_none());
        assert_eq!(
            props["RoleArn"],
            json!("arn:aws:iam::123456789012:role/sfn-role")
        );
    }

    // Fix 3: a SAM StateMachine with a Schedule event expands into an
    // EventBridge rule targeting the state machine + a StartExecution role,
    // instead of dropping `Events`.
    #[test]
    fn expand_sam_statemachine_schedule_event() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "MySM": {
                    "Type": "AWS::Serverless::StateMachine",
                    "Properties": {
                        "Definition": {"StartAt": "Done", "States": {"Done": {"Type": "Succeed"}}},
                        "Events": {
                            "Nightly": {
                                "Type": "Schedule",
                                "Properties": { "Schedule": "rate(1 day)" }
                            }
                        }
                    }
                }
            }
        });

        let expanded = expand_sam(&template).unwrap();
        let resources = &expanded["Resources"];
        // The state machine no longer carries Events.
        assert!(resources["MySM"]["Properties"].get("Events").is_none());
        // A trigger rule was synthesized targeting the state machine.
        let rule = &resources["MySMNightlyRule"];
        assert_eq!(rule["Type"], json!("AWS::Events::Rule"));
        assert_eq!(
            rule["Properties"]["ScheduleExpression"],
            json!("rate(1 day)")
        );
        assert_eq!(
            rule["Properties"]["Targets"][0]["Arn"],
            json!({"Fn::GetAtt": ["MySM", "Arn"]})
        );
        // The shared StartExecution role was synthesized and referenced.
        let role = &resources["MySMEventsRole"];
        assert_eq!(role["Type"], json!("AWS::IAM::Role"));
        assert_eq!(
            rule["Properties"]["Targets"][0]["RoleArn"],
            json!({"Fn::GetAtt": ["MySMEventsRole", "Arn"]})
        );
        assert_eq!(
            role["Properties"]["Policies"][0]["PolicyDocument"]["Statement"][0]["Action"],
            json!("states:StartExecution")
        );
    }

    // Fix 3: an EventBridgeRule state-machine event expands into an
    // EventPattern rule targeting the state machine.
    #[test]
    fn expand_sam_statemachine_eventbridge_event() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "MySM": {
                    "Type": "AWS::Serverless::StateMachine",
                    "Properties": {
                        "Definition": {"StartAt": "Done", "States": {"Done": {"Type": "Succeed"}}},
                        "Events": {
                            "OnOrder": {
                                "Type": "EventBridgeRule",
                                "Properties": { "Pattern": {"source": ["orders"]} }
                            }
                        }
                    }
                }
            }
        });

        let resources = expand_sam(&template).unwrap()["Resources"].clone();
        let rule = &resources["MySMOnOrderRule"];
        assert_eq!(rule["Type"], json!("AWS::Events::Rule"));
        assert_eq!(
            rule["Properties"]["EventPattern"],
            json!({"source": ["orders"]})
        );
        assert_eq!(
            rule["Properties"]["Targets"][0]["Arn"],
            json!({"Fn::GetAtt": ["MySM", "Arn"]})
        );
    }

    // Fix 2: a SAM Connector (Lambda -> DynamoDB, [Read, Write]) expands into
    // an IAM policy on the source function's implicit role granting the CRUD
    // actions on the table, instead of a no-backing phantom.
    #[test]
    fn expand_sam_connector_lambda_to_dynamodb() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "Writer": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": { "Handler": "i.h", "Runtime": "python3.13", "InlineCode": "x" }
                },
                "Table": { "Type": "AWS::Serverless::SimpleTable" },
                "WriterToTable": {
                    "Type": "AWS::Serverless::Connector",
                    "Properties": {
                        "Source": { "Id": "Writer" },
                        "Destination": { "Id": "Table" },
                        "Permissions": ["Read", "Write"]
                    }
                }
            }
        });

        let resources = expand_sam(&template).unwrap()["Resources"].clone();
        // The connector itself is gone; an IAM policy took its place.
        assert!(resources.get("WriterToTable").is_none());
        let policy = &resources["WriterToTablePolicy"];
        assert_eq!(policy["Type"], json!("AWS::IAM::Policy"));
        // Attached to the function's implicit execution role.
        assert_eq!(
            policy["Properties"]["Roles"][0],
            json!({"Ref": "WriterRole"})
        );
        let stmt = &policy["Properties"]["PolicyDocument"]["Statement"][0];
        let actions = stmt["Action"].as_array().unwrap();
        assert!(actions.iter().any(|a| a == "dynamodb:GetItem"));
        assert!(actions.iter().any(|a| a == "dynamodb:PutItem"));
        // Resource scoped to the table + its indexes.
        let resource = stmt["Resource"].as_array().unwrap();
        assert_eq!(resource[0], json!({"Fn::GetAtt": ["Table", "Arn"]}));
    }

    // Fix 2: a SAM Connector write-only permission grants only the write action
    // set (no read actions), proving Permissions is honored.
    #[test]
    fn expand_sam_connector_write_only_sqs() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "Producer": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": { "Handler": "i.h", "Runtime": "python3.13", "InlineCode": "x" }
                },
                "Queue": { "Type": "AWS::SQS::Queue" },
                "ProducerToQueue": {
                    "Type": "AWS::Serverless::Connector",
                    "Properties": {
                        "Source": { "Id": "Producer" },
                        "Destination": { "Id": "Queue", "Type": "AWS::SQS::Queue" },
                        "Permissions": ["Write"]
                    }
                }
            }
        });

        let resources = expand_sam(&template).unwrap()["Resources"].clone();
        let stmt =
            &resources["ProducerToQueuePolicy"]["Properties"]["PolicyDocument"]["Statement"][0];
        let actions = stmt["Action"].as_array().unwrap();
        assert!(actions.iter().any(|a| a == "sqs:SendMessage"));
        assert!(!actions.iter().any(|a| a == "sqs:ReceiveMessage"));
    }

    // Fix 2: a SAM Application expands into a native nested
    // AWS::CloudFormation::Stack pointing at the referenced template, carrying
    // Parameters through, instead of a no-backing phantom.
    #[test]
    fn expand_sam_application_to_nested_stack() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "Nested": {
                    "Type": "AWS::Serverless::Application",
                    "Properties": {
                        "Location": "https://s3.amazonaws.com/bucket/child.template",
                        "Parameters": { "Env": "prod" }
                    }
                }
            }
        });

        let resources = expand_sam(&template).unwrap()["Resources"].clone();
        let stack = &resources["Nested"];
        assert_eq!(stack["Type"], json!("AWS::CloudFormation::Stack"));
        assert_eq!(
            stack["Properties"]["TemplateURL"],
            json!("https://s3.amazonaws.com/bucket/child.template")
        );
        assert_eq!(stack["Properties"]["Parameters"], json!({"Env": "prod"}));
    }

    // Fix 2: an Application whose Location is an S3 object form maps to an
    // s3:// TemplateURL.
    #[test]
    fn expand_sam_application_s3_location_object() {
        let template = json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "Nested": {
                    "Type": "AWS::Serverless::Application",
                    "Properties": {
                        "Location": { "Bucket": "b", "Key": "k/child.yaml" }
                    }
                }
            }
        });

        let resources = expand_sam(&template).unwrap()["Resources"].clone();
        assert_eq!(
            resources["Nested"]["Properties"]["TemplateURL"],
            json!("s3://b/k/child.yaml")
        );
    }

    fn sam(resources: Value) -> Value {
        json!({ "Transform": "AWS::Serverless-2016-10-31", "Resources": resources })
    }

    fn function(events: Value) -> Value {
        json!({
            "Type": "AWS::Serverless::Function",
            "Properties": { "Handler": "i.h", "Runtime": "python3.13", "InlineCode": "x", "Events": events }
        })
    }

    #[test]
    fn unknown_event_type_fails_the_transform() {
        let err = expand_sam(&sam(json!({
            "Fn": function(json!({"E": {"Type": "Carrier", "Properties": {}}}))
        })))
        .unwrap_err();
        assert!(
            err.contains("Event type 'Carrier' is not supported"),
            "{err}"
        );
    }

    #[test]
    fn api_event_must_reference_a_serverless_api_and_have_path_and_method() {
        let err = expand_sam(&sam(json!({
            "Native": {"Type": "AWS::ApiGateway::RestApi", "Properties": {"Name": "n"}},
            "Fn": function(json!({"E": {"Type": "Api", "Properties": {"RestApiId": {"Ref": "Native"}, "Path": "/", "Method": "get"}}}))
        })))
        .unwrap_err();
        assert!(err.contains("RestApiId must be a valid reference"), "{err}");
        let err = expand_sam(&sam(json!({
            "Fn": function(json!({"E": {"Type": "Api", "Properties": {"Path": "/x"}}}))
        })))
        .unwrap_err();
        assert!(err.contains("'Path' or 'Method'"), "{err}");
    }

    #[test]
    fn explicit_rest_api_gets_routes_auth_cors_deployment_and_stage() {
        let expanded = expand_sam(&sam(json!({
            "MyApi": {
                "Type": "AWS::Serverless::Api",
                "Properties": {
                    "StageName": "v1",
                    "Cors": {"AllowOrigin": "'https://example.com'", "AllowHeaders": "'Content-Type'"},
                    "Auth": {
                        "DefaultAuthorizer": "TokenAuth",
                        "AddDefaultAuthorizerToCorsPreflight": false,
                        "Authorizers": {"TokenAuth": {"FunctionArn": {"Fn::GetAtt": ["Authz", "Arn"]}}}
                    }
                }
            },
            "Fn": function(json!({
                "Get": {"Type": "Api", "Properties": {"RestApiId": {"Ref": "MyApi"}, "Path": "/items", "Method": "get"}},
                "Open": {"Type": "Api", "Properties": {"RestApiId": {"Ref": "MyApi"}, "Path": "/open", "Method": "post", "Auth": {"Authorizer": "NONE", "ApiKeyRequired": true}}}
            }))
        })))
        .unwrap();
        let r = &expanded["Resources"];
        let api = &r["MyApi"];
        assert_eq!(api["Type"], "AWS::ApiGateway::RestApi");
        let body = &api["Properties"]["Body"];
        assert_eq!(body["info"]["title"], json!({"Ref": "AWS::StackName"}));
        let get = &body["paths"]["/items"]["get"];
        assert_eq!(get["x-amazon-apigateway-integration"]["type"], "aws_proxy");
        assert_eq!(get["security"], json!([{"TokenAuth": []}]));
        assert_eq!(
            body["paths"]["/open"]["post"]["security"],
            json!([{"api_key": []}])
        );
        let options = &body["paths"]["/items"]["options"];
        let params = &options["x-amazon-apigateway-integration"]["responses"]["default"]
            ["responseParameters"];
        assert_eq!(
            params["method.response.header.Access-Control-Allow-Origin"],
            "'https://example.com'"
        );
        assert_eq!(
            params["method.response.header.Access-Control-Allow-Methods"],
            "'GET,OPTIONS'"
        );
        assert!(options.get("security").is_none());
        assert_eq!(
            body["securityDefinitions"]["TokenAuth"]["x-amazon-apigateway-authorizer"]["type"],
            "token"
        );
        assert_eq!(
            r["MyApiTokenAuthAuthorizerPermission"]["Type"],
            "AWS::Lambda::Permission"
        );
        let stage = &r["MyApiv1Stage"];
        assert_eq!(stage["Properties"]["StageName"], "v1");
        let dep_id = stage["Properties"]["DeploymentId"]["Ref"].as_str().unwrap();
        assert!(dep_id.starts_with("MyApiDeployment"));
        assert_eq!(r[dep_id]["Type"], "AWS::ApiGateway::Deployment");
        assert_eq!(
            r["FnGetPermission"]["Properties"]["Principal"],
            "apigateway.amazonaws.com"
        );
        assert!(r.get("ServerlessRestApi").is_none());
    }

    #[test]
    fn explicit_http_api_routes_with_jwt_authorizer_and_default_stage() {
        let expanded = expand_sam(&sam(json!({
            "Http": {
                "Type": "AWS::Serverless::HttpApi",
                "Properties": {
                    "CorsConfiguration": true,
                    "Auth": {
                        "DefaultAuthorizer": "Jwt",
                        "Authorizers": {"Jwt": {"JwtConfiguration": {"issuer": "https://issuer", "audience": ["a"]}, "IdentitySource": "$request.header.Authorization"}}
                    }
                }
            },
            "Fn": function(json!({
                "Any": {"Type": "HttpApi", "Properties": {"ApiId": {"Ref": "Http"}}},
                "Get": {"Type": "HttpApi", "Properties": {"ApiId": {"Ref": "Http"}, "Path": "/x", "Method": "GET"}}
            }))
        })))
        .unwrap();
        let r = &expanded["Resources"];
        assert_eq!(r["Http"]["Type"], "AWS::ApiGatewayV2::Api");
        assert_eq!(
            r["Http"]["Properties"]["Name"],
            json!({"Ref": "AWS::StackName"})
        );
        assert_eq!(
            r["Http"]["Properties"]["CorsConfiguration"]["AllowOrigins"],
            json!(["*"])
        );
        assert_eq!(
            r["HttpApiGatewayDefaultStage"]["Properties"]["StageName"],
            "$default"
        );
        assert_eq!(r["FnAnyRoute"]["Properties"]["RouteKey"], "$default");
        assert_eq!(r["FnGetRoute"]["Properties"]["RouteKey"], "GET /x");
        assert_eq!(r["FnGetRoute"]["Properties"]["AuthorizationType"], "JWT");
        assert_eq!(
            r["FnGetRoute"]["Properties"]["AuthorizerId"],
            json!({"Ref": "HttpJwtAuthorizer"})
        );
        assert_eq!(
            r["HttpJwtAuthorizer"]["Properties"]["JwtConfiguration"],
            json!({"Issuer": "https://issuer", "Audience": ["a"]})
        );
    }

    #[test]
    fn auto_publish_alias_retargets_events_and_resolves_alias_refs() {
        let expanded = expand_sam(&json!({
            "Transform": "AWS::Serverless-2016-10-31",
            "Resources": {
                "Fn": {
                    "Type": "AWS::Serverless::Function",
                    "Properties": {
                        "Handler": "i.h", "Runtime": "python3.13", "InlineCode": "x",
                        "AutoPublishAlias": "live",
                        "FunctionUrlConfig": {"AuthType": "AWS_IAM"},
                        "Events": {"Q": {"Type": "SQS", "Properties": {"Queue": "arn:aws:sqs:us-east-1:1:q"}}}
                    }
                }
            },
            "Outputs": {
                "Alias": {"Value": {"Ref": "Fn.Alias"}},
                "Version": {"Value": {"Fn::Sub": "${Fn.Version}"}}
            }
        }))
        .unwrap();
        let r = &expanded["Resources"];
        assert_eq!(r["FnAliaslive"]["Type"], "AWS::Lambda::Alias");
        let version_id = r["FnAliaslive"]["Properties"]["FunctionVersion"]["Fn::GetAtt"][0]
            .as_str()
            .unwrap()
            .to_string();
        assert!(version_id.starts_with("FnVersion"));
        assert_eq!(r[&version_id]["DeletionPolicy"], "Retain");
        assert_eq!(
            r["FnQEventSourceMapping"]["Properties"]["FunctionName"],
            json!({"Ref": "FnAliaslive"})
        );
        assert_eq!(r["FnUrl"]["Properties"]["Qualifier"], "live");
        assert!(r.get("FnUrlPublicPermissions").is_none());
        assert!(r["Fn"]["Properties"].get("AutoPublishAlias").is_none());
        assert_eq!(
            expanded["Outputs"]["Alias"]["Value"],
            json!({"Ref": "FnAliaslive"})
        );
        assert_eq!(
            expanded["Outputs"]["Version"]["Value"],
            json!({"Fn::Sub": format!("${{{version_id}}}")})
        );
    }

    #[test]
    fn alias_dependent_properties_require_auto_publish_alias() {
        for (key, value) in [
            ("DeploymentPreference", json!({"Type": "AllAtOnce"})),
            (
                "ProvisionedConcurrencyConfig",
                json!({"ProvisionedConcurrentExecutions": 1}),
            ),
        ] {
            let mut f = function(json!({}));
            f["Properties"][key] = value;
            let err = expand_sam(&sam(json!({ "Fn": f }))).unwrap_err();
            assert!(
                err.contains("AutoPublishAlias") || err.contains("AutoPublishALias"),
                "{err}"
            );
        }
    }

    #[test]
    fn s3_and_cognito_events_edit_the_referenced_resources() {
        let expanded = expand_sam(&sam(json!({
            "Bucket": {"Type": "AWS::S3::Bucket"},
            "Pool": {"Type": "AWS::Cognito::UserPool"},
            "Fn": function(json!({
                "Up": {"Type": "S3", "Properties": {"Bucket": {"Ref": "Bucket"}, "Events": ["s3:ObjectCreated:*"], "Filter": {"S3Key": {"Rules": [{"Name": "prefix", "Value": "in/"}]}}}},
                "Sign": {"Type": "Cognito", "Properties": {"UserPool": {"Ref": "Pool"}, "Trigger": "PreSignUp"}}
            }))
        })))
        .unwrap();
        let r = &expanded["Resources"];
        let lc = &r["Bucket"]["Properties"]["NotificationConfiguration"]["LambdaConfigurations"][0];
        assert_eq!(lc["Event"], "s3:ObjectCreated:*");
        assert_eq!(lc["Function"], json!({"Fn::GetAtt": ["Fn", "Arn"]}));
        assert_eq!(r["Bucket"]["DependsOn"], json!(["FnUpPermission"]));
        assert_eq!(
            r["FnUpPermission"]["Properties"]["SourceAccount"],
            json!({"Ref": "AWS::AccountId"})
        );
        assert_eq!(
            r["Pool"]["Properties"]["LambdaConfig"]["PreSignUp"],
            json!({"Fn::GetAtt": ["Fn", "Arn"]})
        );

        let err = expand_sam(&sam(json!({
            "Fn": function(json!({"Up": {"Type": "S3", "Properties": {"Bucket": "literal", "Events": "s3:ObjectCreated:*"}}}))
        })))
        .unwrap_err();
        assert!(err.contains("S3 bucket in the same template"), "{err}");
    }
}
