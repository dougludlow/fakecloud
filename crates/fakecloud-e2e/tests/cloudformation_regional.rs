//! End-to-end tests for CloudFormation regionality: stacks, exports and stack
//! sets live in the region they were created in, as in AWS.

mod helpers;

use aws_sdk_cloudformation::error::ProvideErrorMetadata;
use aws_sdk_cloudformation::types::{StackSetOperationStatus, StackStatus};
use helpers::TestServer;

const EAST: &str = "us-east-1";
const WEST: &str = "eu-west-1";

async fn cfn_in(server: &TestServer, region: &str) -> aws_sdk_cloudformation::Client {
    aws_sdk_cloudformation::Client::new(&server.aws_config_in(region).await)
}

fn handle_template(outputs: &str) -> String {
    format!(
        r#"{{
            "Resources": {{"Handle": {{"Type": "AWS::CloudFormation::WaitConditionHandle"}}}},
            "Outputs": {{{outputs}}}
        }}"#
    )
}

async fn create(cfn: &aws_sdk_cloudformation::Client, name: &str, template: &str) -> String {
    cfn.create_stack()
        .stack_name(name)
        .template_body(template)
        .send()
        .await
        .unwrap_or_else(|e| panic!("create_stack {name}: {e:?}"))
        .stack_id()
        .expect("stack id")
        .to_string()
}

async fn stack_ids(cfn: &aws_sdk_cloudformation::Client) -> Vec<String> {
    cfn.list_stacks()
        .send()
        .await
        .unwrap()
        .stack_summaries()
        .iter()
        .filter(|s| s.stack_status() != Some(&StackStatus::DeleteComplete))
        .filter_map(|s| s.stack_id().map(str::to_string))
        .collect()
}

async fn output(cfn: &aws_sdk_cloudformation::Client, stack: &str, key: &str) -> Option<String> {
    cfn.describe_stacks()
        .stack_name(stack)
        .send()
        .await
        .unwrap()
        .stacks()
        .first()
        .expect("stack")
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some(key))
        .and_then(|o| o.output_value().map(str::to_string))
}

#[tokio::test]
async fn the_same_stack_name_lives_independently_in_two_regions() {
    let server = TestServer::start().await;
    let east = cfn_in(&server, EAST).await;
    let west = cfn_in(&server, WEST).await;
    let template = handle_template(r#""Region": {"Value": {"Ref": "AWS::Region"}}"#);

    let east_id = create(&east, "app", &template).await;
    let west_id = create(&west, "app", &template).await;
    assert!(
        east_id.starts_with("arn:aws:cloudformation:us-east-1:123456789012:stack/app/"),
        "{east_id}"
    );
    assert!(
        west_id.starts_with("arn:aws:cloudformation:eu-west-1:123456789012:stack/app/"),
        "{west_id}"
    );

    // The name is still unique within a region.
    let dup = west
        .create_stack()
        .stack_name("app")
        .template_body(&template)
        .send()
        .await
        .expect_err("duplicate in the same region");
    assert_eq!(dup.code(), Some("AlreadyExistsException"));

    // Each region lists and describes only its own stack.
    assert_eq!(stack_ids(&east).await, vec![east_id.clone()]);
    assert_eq!(stack_ids(&west).await, vec![west_id.clone()]);
    let all_east = east.describe_stacks().send().await.unwrap();
    assert_eq!(all_east.stacks().len(), 1);
    assert_eq!(all_east.stacks()[0].stack_id(), Some(east_id.as_str()));
    assert_eq!(output(&east, "app", "Region").await.as_deref(), Some(EAST));
    assert_eq!(output(&west, "app", "Region").await.as_deref(), Some(WEST));

    // A stack id only resolves in the region it names.
    let e = east
        .describe_stacks()
        .stack_name(&west_id)
        .send()
        .await
        .expect_err("eu-west-1 stack from us-east-1");
    assert_eq!(e.code(), Some("ValidationError"));
    assert_eq!(
        e.message(),
        Some(format!("Stack with id {west_id} does not exist").as_str())
    );
    let resources = west
        .list_stack_resources()
        .stack_name(&west_id)
        .send()
        .await
        .unwrap();
    assert_eq!(resources.stack_resource_summaries().len(), 1);

    // Deleting one leaves the other.
    east.delete_stack().stack_name("app").send().await.unwrap();
    assert!(stack_ids(&east).await.is_empty());
    let remaining = west
        .describe_stacks()
        .stack_name("app")
        .send()
        .await
        .unwrap();
    let stack = remaining.stacks().first().expect("eu-west-1 stack");
    assert_eq!(stack.stack_id(), Some(west_id.as_str()));
    assert_eq!(stack.stack_status(), Some(&StackStatus::CreateComplete));
}

#[tokio::test]
async fn import_value_resolves_exports_of_the_stack_region_only() {
    let server = TestServer::start().await;
    let east = cfn_in(&server, EAST).await;
    let west = cfn_in(&server, WEST).await;

    // The same export name in both regions is not a collision.
    let producer = |value: &str| {
        handle_template(&format!(
            r#""Out": {{"Value": "{value}", "Export": {{"Name": "Shared"}}}}"#
        ))
    };
    create(&east, "producer", &producer("east-value")).await;
    create(&west, "producer", &producer("west-value")).await;

    let consumer = handle_template(r#""Imported": {"Value": {"Fn::ImportValue": "Shared"}}"#);
    create(&east, "consumer", &consumer).await;
    create(&west, "consumer", &consumer).await;
    assert_eq!(
        output(&east, "consumer", "Imported").await.as_deref(),
        Some("east-value")
    );
    assert_eq!(
        output(&west, "consumer", "Imported").await.as_deref(),
        Some("west-value")
    );

    // ListExports / ListImports report the request region.
    for (cfn, value) in [(&east, "east-value"), (&west, "west-value")] {
        let exports = cfn.list_exports().send().await.unwrap();
        let values: Vec<&str> = exports.exports().iter().filter_map(|e| e.value()).collect();
        assert_eq!(values, vec![value]);
        let imports = cfn
            .list_imports()
            .export_name("Shared")
            .send()
            .await
            .unwrap();
        assert_eq!(imports.imports(), ["consumer".to_string()]);
    }

    // An export only another region has is not found.
    create(
        &east,
        "east-only",
        &handle_template(r#""Out": {"Value": "v", "Export": {"Name": "EastOnly"}}"#),
    )
    .await;
    let e = west
        .create_stack()
        .stack_name("importer")
        .template_body(handle_template(
            r#""Imported": {"Value": {"Fn::ImportValue": "EastOnly"}}"#,
        ))
        .send()
        .await
        .expect_err("export of another region");
    assert_eq!(e.message(), Some("No export named EastOnly found."));
}

#[tokio::test]
async fn stack_set_instances_create_and_delete_stacks_in_their_regions() {
    let server = TestServer::start().await;
    let east = cfn_in(&server, EAST).await;
    let west = cfn_in(&server, WEST).await;

    east.create_stack_set()
        .stack_set_name("multi")
        .template_body(handle_template(
            r#""Region": {"Value": {"Ref": "AWS::Region"}}"#,
        ))
        .send()
        .await
        .unwrap();
    // The stack set itself lives in its administration region.
    let e = west
        .describe_stack_set()
        .stack_set_name("multi")
        .send()
        .await
        .expect_err("stack set from another region");
    assert_eq!(e.code(), Some("StackSetNotFoundException"));
    assert!(west
        .list_stack_sets()
        .send()
        .await
        .unwrap()
        .summaries()
        .is_empty());

    let op = east
        .create_stack_instances()
        .stack_set_name("multi")
        .accounts("123456789012")
        .regions(EAST)
        .regions(WEST)
        .send()
        .await
        .unwrap();
    let op_id = op.operation_id().expect("operation id").to_string();
    wait_for_operation(&east, "multi", &op_id).await;

    // One stack per region, each visible only in its own region.
    let instance_stack = |region: &'static str| {
        let east = east.clone();
        async move {
            east.describe_stack_instance()
                .stack_set_name("multi")
                .stack_instance_account("123456789012")
                .stack_instance_region(region)
                .send()
                .await
                .unwrap()
                .stack_instance()
                .and_then(|i| i.stack_id())
                .expect("instance stack id")
                .to_string()
        }
    };
    let east_stack = instance_stack(EAST).await;
    let west_stack = instance_stack(WEST).await;
    assert!(east_stack.contains(":us-east-1:"), "{east_stack}");
    assert!(west_stack.contains(":eu-west-1:"), "{west_stack}");
    assert_eq!(stack_ids(&east).await, vec![east_stack.clone()]);
    assert_eq!(stack_ids(&west).await, vec![west_stack.clone()]);
    assert_eq!(
        output(&west, &west_stack, "Region").await.as_deref(),
        Some(WEST)
    );

    // Deleting the eu-west-1 instance deletes the stack there and only there.
    let op = east
        .delete_stack_instances()
        .stack_set_name("multi")
        .accounts("123456789012")
        .regions(WEST)
        .retain_stacks(false)
        .send()
        .await
        .unwrap();
    let op_id = op.operation_id().expect("operation id").to_string();
    wait_for_operation(&east, "multi", &op_id).await;
    assert!(stack_ids(&west).await.is_empty());
    assert_eq!(stack_ids(&east).await, vec![east_stack]);
}

async fn wait_for_operation(cfn: &aws_sdk_cloudformation::Client, set: &str, op_id: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = cfn
            .describe_stack_set_operation()
            .stack_set_name(set)
            .operation_id(op_id)
            .send()
            .await
            .unwrap()
            .stack_set_operation()
            .and_then(|op| op.status())
            .cloned()
            .expect("operation status");
        match status {
            StackSetOperationStatus::Succeeded => return,
            StackSetOperationStatus::Running
            | StackSetOperationStatus::Queued
            | StackSetOperationStatus::Stopping => {}
            other => panic!("operation {op_id} ended {other:?}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "operation {op_id} still {status:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// A stack id (ARN) only addresses a stack in its own region. Sent to another
/// region, `DeleteStack`, `UpdateStack` and `CreateChangeSet` all answer that
/// the stack does not exist, and touch neither region's stacks.
#[tokio::test]
async fn a_stack_id_from_another_region_is_refused() {
    let server = TestServer::start().await;
    let east = cfn_in(&server, EAST).await;
    let west = cfn_in(&server, WEST).await;
    let template = handle_template(r#""Region": {"Value": {"Ref": "AWS::Region"}}"#);
    let west_id = create(&west, "app", &template).await;
    let east_id = create(&east, "app", &template).await;
    let not_found = format!("Stack with id {west_id} does not exist");

    let e = east
        .delete_stack()
        .stack_name(&west_id)
        .send()
        .await
        .expect_err("DeleteStack with an eu-west-1 id in us-east-1");
    assert_eq!(e.code(), Some("ValidationError"));
    assert_eq!(e.message(), Some(not_found.as_str()));

    let e = east
        .update_stack()
        .stack_name(&west_id)
        .template_body(&template)
        .send()
        .await
        .expect_err("UpdateStack with an eu-west-1 id in us-east-1");
    assert_eq!(e.code(), Some("ValidationError"));
    assert_eq!(e.message(), Some(not_found.as_str()));

    for change_set_type in [
        aws_sdk_cloudformation::types::ChangeSetType::Update,
        aws_sdk_cloudformation::types::ChangeSetType::Create,
    ] {
        let e = east
            .create_change_set()
            .stack_name(&west_id)
            .change_set_name("cs")
            .change_set_type(change_set_type.clone())
            .template_body(&template)
            .send()
            .await
            .expect_err("CreateChangeSet with an eu-west-1 id in us-east-1");
        assert_eq!(e.code(), Some("ValidationError"), "{change_set_type:?}");
        assert_eq!(e.message(), Some(not_found.as_str()));
    }

    // Both stacks are untouched, and no change set or extra stack appeared.
    assert_eq!(stack_ids(&east).await, vec![east_id]);
    assert_eq!(stack_ids(&west).await, vec![west_id]);
    for cfn in [&east, &west] {
        let change_sets = cfn
            .list_change_sets()
            .stack_name("app")
            .send()
            .await
            .unwrap();
        assert!(change_sets.summaries().is_empty());
        let stack = cfn
            .describe_stacks()
            .stack_name("app")
            .send()
            .await
            .unwrap();
        assert_eq!(
            stack.stacks()[0].stack_status(),
            Some(&StackStatus::CreateComplete)
        );
    }
}
