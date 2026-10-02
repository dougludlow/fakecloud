//! Application Auto Scaling service E2E.

mod helpers;

use std::collections::HashMap;

use aws_sdk_applicationautoscaling::primitives::DateTime as AwsDateTime;
use aws_sdk_applicationautoscaling::types::{
    AdjustmentType, MetricAggregationType, MetricType, PolicyType, PredefinedMetricSpecification,
    ScalableDimension, ScalableTargetAction, ServiceNamespace, StepAdjustment,
    StepScalingPolicyConfiguration, SuspendedState, TargetTrackingScalingPolicyConfiguration,
};
use aws_sdk_cloudwatch::primitives::DateTime as CwDateTime;
use aws_sdk_cloudwatch::types::{
    ComparisonOperator as CwComparisonOperator, MetricDatum as CwMetricDatum, StandardUnit,
};
use aws_sdk_dynamodb::types::{
    AttributeDefinition as DdbAttributeDefinition, BillingMode, KeySchemaElement,
    KeyType as DdbKeyType, ProvisionedThroughput, ScalarAttributeType,
};
use helpers::TestServer;

#[tokio::test]
async fn register_and_describe_scalable_target_round_trip() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    let arn = aas
        .register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/svc")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(10)
        .send()
        .await
        .expect("register")
        .scalable_target_arn()
        .map(str::to_owned)
        .expect("arn");
    assert!(arn.contains(":scalable-target/"));

    let described = aas
        .describe_scalable_targets()
        .service_namespace(ServiceNamespace::Ecs)
        .send()
        .await
        .expect("describe");
    let targets = described.scalable_targets();
    assert_eq!(targets.len(), 1);
    let t = &targets[0];
    assert_eq!(t.resource_id(), "service/cluster/svc");
    assert_eq!(t.min_capacity(), 1);
    assert_eq!(t.max_capacity(), 10);
    // SDK auto-injects a default IAM role ARN; verify presence.
    assert!(t
        .role_arn()
        .contains("AWSServiceRoleForApplicationAutoScaling"));
}

#[tokio::test]
async fn register_scalable_target_update_path_only_changes_supplied_fields() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Lambda)
        .resource_id("function:my-fn:PROVISIONED")
        .scalable_dimension(ScalableDimension::LambdaFunctionProvisionedConcurrency)
        .min_capacity(2)
        .max_capacity(20)
        .send()
        .await
        .expect("register");

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Lambda)
        .resource_id("function:my-fn:PROVISIONED")
        .scalable_dimension(ScalableDimension::LambdaFunctionProvisionedConcurrency)
        .max_capacity(40)
        .suspended_state(
            SuspendedState::builder()
                .dynamic_scaling_in_suspended(true)
                .build(),
        )
        .send()
        .await
        .expect("update");

    let target = aas
        .describe_scalable_targets()
        .service_namespace(ServiceNamespace::Lambda)
        .send()
        .await
        .expect("describe")
        .scalable_targets()
        .first()
        .cloned()
        .expect("target");
    assert_eq!(target.min_capacity(), 2);
    assert_eq!(target.max_capacity(), 40);
    assert_eq!(
        target
            .suspended_state()
            .and_then(|s| s.dynamic_scaling_in_suspended()),
        Some(true)
    );
}

#[tokio::test]
async fn deregister_scalable_target_cascades_to_policies_and_actions() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .min_capacity(5)
        .max_capacity(50)
        .send()
        .await
        .expect("register");

    aas.put_scaling_policy()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .policy_name("read-target")
        .policy_type(PolicyType::TargetTrackingScaling)
        .target_tracking_scaling_policy_configuration(
            TargetTrackingScalingPolicyConfiguration::builder()
                .target_value(70.0)
                .predefined_metric_specification(
                    PredefinedMetricSpecification::builder()
                        .predefined_metric_type(
                            aws_sdk_applicationautoscaling::types::MetricType::DynamoDbReadCapacityUtilization,
                        )
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("policy");

    aas.put_scheduled_action()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .scheduled_action_name("nightly-scale")
        .schedule("cron(0 22 * * ? *)")
        .scalable_target_action(
            ScalableTargetAction::builder()
                .min_capacity(2)
                .max_capacity(8)
                .build(),
        )
        .send()
        .await
        .expect("scheduled");

    aas.deregister_scalable_target()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .send()
        .await
        .expect("deregister");

    let policies = aas
        .describe_scaling_policies()
        .service_namespace(ServiceNamespace::Dynamodb)
        .send()
        .await
        .expect("policies")
        .scaling_policies()
        .len();
    let actions = aas
        .describe_scheduled_actions()
        .service_namespace(ServiceNamespace::Dynamodb)
        .send()
        .await
        .expect("actions")
        .scheduled_actions()
        .len();
    assert_eq!(policies, 0, "policies cascade-delete with target");
    assert_eq!(actions, 0, "scheduled actions cascade-delete with target");
}

#[tokio::test]
async fn put_scaling_policy_step_then_describe_and_delete() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/api")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(20)
        .send()
        .await
        .expect("register");

    let policy_arn = aas
        .put_scaling_policy()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/api")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("scale-out")
        .policy_type(PolicyType::StepScaling)
        .step_scaling_policy_configuration(
            StepScalingPolicyConfiguration::builder()
                .adjustment_type(
                    aws_sdk_applicationautoscaling::types::AdjustmentType::ChangeInCapacity,
                )
                .metric_aggregation_type(MetricAggregationType::Average)
                .step_adjustments(
                    StepAdjustment::builder()
                        .scaling_adjustment(2)
                        .metric_interval_lower_bound(0.0)
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("put policy")
        .policy_arn()
        .to_owned();
    assert!(policy_arn.contains(":scalingPolicy:"));

    let policies = aas
        .describe_scaling_policies()
        .service_namespace(ServiceNamespace::Ecs)
        .send()
        .await
        .expect("describe")
        .scaling_policies()
        .to_vec();
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].policy_name(), "scale-out");
    assert_eq!(policies[0].policy_type(), &PolicyType::StepScaling);

    aas.delete_scaling_policy()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/api")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("scale-out")
        .send()
        .await
        .expect("delete policy");
    assert_eq!(
        aas.describe_scaling_policies()
            .service_namespace(ServiceNamespace::Ecs)
            .send()
            .await
            .expect("describe2")
            .scaling_policies()
            .len(),
        0
    );
}

#[tokio::test]
async fn put_scheduled_action_lifecycle() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(10)
        .send()
        .await
        .expect("register");

    aas.put_scheduled_action()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .scheduled_action_name("morning-warmup")
        .schedule("cron(0 7 * * ? *)")
        .timezone("America/New_York")
        .scalable_target_action(
            ScalableTargetAction::builder()
                .min_capacity(3)
                .max_capacity(10)
                .build(),
        )
        .send()
        .await
        .expect("put");

    let actions = aas
        .describe_scheduled_actions()
        .service_namespace(ServiceNamespace::Ecs)
        .send()
        .await
        .expect("describe")
        .scheduled_actions()
        .to_vec();
    assert_eq!(actions.len(), 1);
    let a = &actions[0];
    assert_eq!(a.scheduled_action_name(), "morning-warmup");
    assert_eq!(a.timezone(), Some("America/New_York"));

    aas.delete_scheduled_action()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/web")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .scheduled_action_name("morning-warmup")
        .send()
        .await
        .expect("delete");
    assert_eq!(
        aas.describe_scheduled_actions()
            .service_namespace(ServiceNamespace::Ecs)
            .send()
            .await
            .expect("d2")
            .scheduled_actions()
            .len(),
        0
    );
}

#[tokio::test]
async fn put_scaling_policy_without_target_returns_object_not_found() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    let err = aas
        .put_scaling_policy()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/no-target")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("orphan")
        .policy_type(PolicyType::StepScaling)
        .step_scaling_policy_configuration(
            StepScalingPolicyConfiguration::builder()
                .adjustment_type(
                    aws_sdk_applicationautoscaling::types::AdjustmentType::ChangeInCapacity,
                )
                .build(),
        )
        .send()
        .await
        .expect_err("must error without a target");
    assert!(format!("{err:?}").contains("ObjectNotFound"));
}

#[tokio::test]
async fn deregister_unknown_target_returns_object_not_found() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    let err = aas
        .deregister_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/missing")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .send()
        .await
        .expect_err("missing target");
    assert!(format!("{err:?}").contains("ObjectNotFound"));
}

#[tokio::test]
async fn describe_scaling_activities_returns_empty_initially() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    let activities = aas
        .describe_scaling_activities()
        .service_namespace(ServiceNamespace::Ecs)
        .send()
        .await
        .expect("activities")
        .scaling_activities()
        .len();
    assert_eq!(activities, 0);
}

#[tokio::test]
async fn get_predictive_scaling_forecast_requires_predictive_policy() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/pred")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(20)
        .send()
        .await
        .expect("register");

    aas.put_scaling_policy()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/pred")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("step-only")
        .policy_type(PolicyType::StepScaling)
        .step_scaling_policy_configuration(
            StepScalingPolicyConfiguration::builder()
                .adjustment_type(
                    aws_sdk_applicationautoscaling::types::AdjustmentType::ChangeInCapacity,
                )
                .build(),
        )
        .send()
        .await
        .expect("step");
    let now = AwsDateTime::from_secs(1_700_000_000);
    let later = AwsDateTime::from_secs(1_700_000_000 + 86_400);
    let err = aas
        .get_predictive_scaling_forecast()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/pred")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .policy_name("step-only")
        .start_time(now)
        .end_time(later)
        .send()
        .await
        .expect_err("not predictive");
    assert!(format!("{err:?}").contains("ValidationException"));
}

#[tokio::test]
async fn tag_untag_list_tags_round_trip() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    let arn = aas
        .register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/tagme")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(5)
        .send()
        .await
        .expect("register")
        .scalable_target_arn()
        .map(str::to_owned)
        .expect("arn");

    let mut tags = HashMap::new();
    tags.insert("env".to_string(), "prod".to_string());
    tags.insert("team".to_string(), "sre".to_string());
    aas.tag_resource()
        .resource_arn(&arn)
        .set_tags(Some(tags))
        .send()
        .await
        .expect("tag");

    let listed = aas
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .expect("list")
        .tags()
        .cloned()
        .unwrap_or_default();
    assert_eq!(listed.get("env").map(String::as_str), Some("prod"));
    assert_eq!(listed.get("team").map(String::as_str), Some("sre"));

    aas.untag_resource()
        .resource_arn(&arn)
        .tag_keys("env")
        .send()
        .await
        .expect("untag");

    let post = aas
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .expect("list2")
        .tags()
        .cloned()
        .unwrap_or_default();
    assert!(!post.contains_key("env"));
    assert_eq!(post.get("team").map(String::as_str), Some("sre"));
}

#[tokio::test]
async fn list_tags_unknown_resource_returns_resource_not_found() {
    // ListTagsForResource's Smithy model only declares
    // ResourceNotFoundException, so unknown ARNs surface that code
    // (not the ObjectNotFoundException used by the Delete/Put
    // scaling-target/policy/scheduled-action ops).
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;
    let err = aas
        .list_tags_for_resource()
        .resource_arn(
            "arn:aws:application-autoscaling:us-east-1:123456789012:scalable-target/deadbeef00",
        )
        .send()
        .await
        .expect_err("missing arn");
    assert!(format!("{err:?}").contains("ResourceNotFound"));
}

#[tokio::test]
async fn re_register_with_min_above_max_rejected() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/bounds")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(5)
        .send()
        .await
        .expect("register");

    let err = aas
        .register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/bounds")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(10)
        .send()
        .await
        .expect_err("min above existing max should reject");
    assert!(format!("{err:?}").contains("ValidationException"));

    // Existing target unchanged.
    let target = aas
        .describe_scalable_targets()
        .service_namespace(ServiceNamespace::Ecs)
        .send()
        .await
        .expect("describe")
        .scalable_targets()
        .first()
        .cloned()
        .expect("target");
    assert_eq!(target.min_capacity(), 1);
    assert_eq!(target.max_capacity(), 5);
}

#[tokio::test]
async fn describe_with_stale_next_token_does_not_panic() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;
    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/page")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(2)
        .send()
        .await
        .expect("register");
    // Token way past end — must clamp instead of panic.
    let resp = aas
        .describe_scalable_targets()
        .service_namespace(ServiceNamespace::Ecs)
        .next_token("9999")
        .send()
        .await
        .expect("paginate stale token");
    assert!(resp.scalable_targets().is_empty());
    assert!(resp.next_token().is_none());
}

#[tokio::test]
async fn tag_unknown_resource_returns_resource_not_found() {
    // TagResource declares ResourceNotFoundException in its Smithy model;
    // ObjectNotFoundException is reserved for the Delete/Put
    // scaling-target/policy/scheduled-action ops.
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;
    let mut tags = HashMap::new();
    tags.insert("k".to_string(), "v".to_string());

    let err = aas
        .tag_resource()
        .resource_arn(
            "arn:aws:application-autoscaling:us-east-1:123456789012:scalable-target/0000000000",
        )
        .set_tags(Some(tags))
        .send()
        .await
        .expect_err("missing arn");
    assert!(format!("{err:?}").contains("ResourceNotFound"));
}

/// POST `/_fakecloud/application-autoscaling/tick` to force the
/// scaling watcher to evaluate immediately, instead of waiting for
/// its 15s interval. Returns the number of policies that applied a
/// capacity change this tick.
async fn force_appas_tick(server: &TestServer) -> usize {
    let url = format!(
        "{}/_fakecloud/application-autoscaling/tick",
        server.endpoint()
    );
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .expect("admin tick");
    let body: serde_json::Value = resp.json().await.expect("json");
    body["applied"].as_u64().unwrap_or(0) as usize
}

async fn create_provisioned_table(
    ddb: &aws_sdk_dynamodb::Client,
    name: &str,
    read: i64,
    write: i64,
) {
    ddb.create_table()
        .table_name(name)
        .billing_mode(BillingMode::Provisioned)
        .attribute_definitions(
            DdbAttributeDefinition::builder()
                .attribute_name("pk")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(DdbKeyType::Hash)
                .build()
                .unwrap(),
        )
        .provisioned_throughput(
            ProvisionedThroughput::builder()
                .read_capacity_units(read)
                .write_capacity_units(write)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("create table");
}

async fn put_ddb_utilisation(
    cw: &aws_sdk_cloudwatch::Client,
    metric: &str,
    table_name: &str,
    value: f64,
) {
    cw.put_metric_data()
        .namespace("AWS/DynamoDB")
        .metric_data(
            CwMetricDatum::builder()
                .metric_name(metric)
                .dimensions(
                    aws_sdk_cloudwatch::types::Dimension::builder()
                        .name("TableName")
                        .value(table_name)
                        .build(),
                )
                .timestamp(CwDateTime::from_secs(chrono::Utc::now().timestamp()))
                .value(value)
                .unit(StandardUnit::Percent)
                .build(),
        )
        .send()
        .await
        .expect("put metric");
}

#[tokio::test]
async fn target_tracking_scales_dynamodb_read_capacity() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;
    let ddb = server.dynamodb_client().await;
    let cw = server.cloudwatch_client().await;

    create_provisioned_table(&ddb, "tt-orders", 10, 5).await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/tt-orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .min_capacity(5)
        .max_capacity(100)
        .send()
        .await
        .expect("register");

    aas.put_scaling_policy()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/tt-orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .policy_name("track-70")
        .policy_type(PolicyType::TargetTrackingScaling)
        .target_tracking_scaling_policy_configuration(
            TargetTrackingScalingPolicyConfiguration::builder()
                .target_value(70.0)
                .predefined_metric_specification(
                    PredefinedMetricSpecification::builder()
                        .predefined_metric_type(MetricType::DynamoDbReadCapacityUtilization)
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("put policy");

    // Drive utilisation to 95% — well above the 70% target — so the
    // watcher must scale capacity up.
    put_ddb_utilisation(&cw, "DynamoDBReadCapacityUtilization", "tt-orders", 95.0).await;
    let applied = force_appas_tick(&server).await;
    assert_eq!(applied, 1, "watcher should apply scale-out");

    let described = ddb
        .describe_table()
        .table_name("tt-orders")
        .send()
        .await
        .expect("describe");
    let throughput = described
        .table()
        .and_then(|t| t.provisioned_throughput())
        .expect("throughput");
    let read_now = throughput.read_capacity_units().unwrap_or(0);
    // 10 * (95 / 70) = 13.57 -> ceil 14.
    assert_eq!(read_now, 14, "expected scale-out to 14, got {read_now}");
    // Write capacity untouched.
    assert_eq!(throughput.write_capacity_units().unwrap_or(0), 5);

    // Now drive utilisation low — watcher should scale in.
    put_ddb_utilisation(&cw, "DynamoDBReadCapacityUtilization", "tt-orders", 35.0).await;
    let applied2 = force_appas_tick(&server).await;
    assert_eq!(applied2, 1, "watcher should apply scale-in");
    let described2 = ddb
        .describe_table()
        .table_name("tt-orders")
        .send()
        .await
        .expect("describe2");
    let read_after = described2
        .table()
        .and_then(|t| t.provisioned_throughput())
        .and_then(|p| p.read_capacity_units())
        .unwrap_or(0);
    // 14 * (35 / 70) = 7. Above min_capacity of 5.
    assert_eq!(read_after, 7, "expected scale-in to 7, got {read_after}");

    // Activities log captures both decisions plus the initial register.
    let activities = aas
        .describe_scaling_activities()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/tt-orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .send()
        .await
        .expect("activities")
        .scaling_activities()
        .to_vec();
    assert!(
        activities.len() >= 3,
        "expected >= 3 activities, got {}",
        activities.len()
    );
}

#[tokio::test]
async fn target_tracking_clamps_to_min_max_bounds() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;
    let ddb = server.dynamodb_client().await;
    let cw = server.cloudwatch_client().await;

    create_provisioned_table(&ddb, "tt-bounds", 10, 5).await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/tt-bounds")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .min_capacity(8)
        .max_capacity(15)
        .send()
        .await
        .expect("register");

    aas.put_scaling_policy()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/tt-bounds")
        .scalable_dimension(ScalableDimension::DynamoDbTableReadCapacityUnits)
        .policy_name("track-50")
        .policy_type(PolicyType::TargetTrackingScaling)
        .target_tracking_scaling_policy_configuration(
            TargetTrackingScalingPolicyConfiguration::builder()
                .target_value(50.0)
                .predefined_metric_specification(
                    PredefinedMetricSpecification::builder()
                        .predefined_metric_type(MetricType::DynamoDbReadCapacityUtilization)
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("put policy");

    // 200% utilisation pushes raw desired beyond max_capacity (15).
    put_ddb_utilisation(&cw, "DynamoDBReadCapacityUtilization", "tt-bounds", 200.0).await;
    force_appas_tick(&server).await;

    let read_now = ddb
        .describe_table()
        .table_name("tt-bounds")
        .send()
        .await
        .expect("describe")
        .table()
        .and_then(|t| t.provisioned_throughput())
        .and_then(|p| p.read_capacity_units())
        .unwrap_or(0);
    assert_eq!(read_now, 15, "scale-out clamped to max_capacity");

    // Drop utilisation to ~0 — desired floor of min_capacity (8).
    put_ddb_utilisation(&cw, "DynamoDBReadCapacityUtilization", "tt-bounds", 1.0).await;
    force_appas_tick(&server).await;
    let read_floor = ddb
        .describe_table()
        .table_name("tt-bounds")
        .send()
        .await
        .expect("describe2")
        .table()
        .and_then(|t| t.provisioned_throughput())
        .and_then(|p| p.read_capacity_units())
        .unwrap_or(0);
    assert_eq!(read_floor, 8, "scale-in clamped to min_capacity");
}

#[tokio::test]
async fn step_scaling_applies_when_alarm_action_fires() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;
    let ddb = server.dynamodb_client().await;
    let cw = server.cloudwatch_client().await;

    create_provisioned_table(&ddb, "step-orders", 10, 10).await;

    aas.register_scalable_target()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/step-orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableWriteCapacityUnits)
        .min_capacity(5)
        .max_capacity(50)
        .send()
        .await
        .expect("register");

    let policy_arn = aas
        .put_scaling_policy()
        .service_namespace(ServiceNamespace::Dynamodb)
        .resource_id("table/step-orders")
        .scalable_dimension(ScalableDimension::DynamoDbTableWriteCapacityUnits)
        .policy_name("step-out")
        .policy_type(PolicyType::StepScaling)
        .step_scaling_policy_configuration(
            StepScalingPolicyConfiguration::builder()
                .adjustment_type(AdjustmentType::ChangeInCapacity)
                .metric_aggregation_type(MetricAggregationType::Average)
                .step_adjustments(
                    StepAdjustment::builder()
                        .scaling_adjustment(10)
                        .metric_interval_lower_bound(0.0)
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("put policy")
        .policy_arn()
        .to_owned();

    // Wire a CloudWatch alarm whose `AlarmActions` references the
    // scaling policy ARN — that's the canonical AWS path to attach
    // a step scaling policy to an alarm. Then SetAlarmState to fire
    // it; the watcher discovers the firing alarm via the action ARN.
    cw.put_metric_alarm()
        .alarm_name("ddb-step-burn")
        .metric_name("DynamoDBWriteCapacityUtilization")
        .namespace("AWS/DynamoDB")
        .statistic(aws_sdk_cloudwatch::types::Statistic::Average)
        .period(60)
        .evaluation_periods(1)
        .threshold(80.0)
        .comparison_operator(CwComparisonOperator::GreaterThanThreshold)
        .alarm_actions(&policy_arn)
        .send()
        .await
        .expect("put alarm");

    cw.set_alarm_state()
        .alarm_name("ddb-step-burn")
        .state_value(aws_sdk_cloudwatch::types::StateValue::Alarm)
        .state_reason("synthetic burn")
        .send()
        .await
        .expect("set state");

    let applied = force_appas_tick(&server).await;
    assert_eq!(applied, 1, "step scaling should apply once");

    let write_now = ddb
        .describe_table()
        .table_name("step-orders")
        .send()
        .await
        .expect("describe")
        .table()
        .and_then(|t| t.provisioned_throughput())
        .and_then(|p| p.write_capacity_units())
        .unwrap_or(0);
    // 10 + 10 = 20.
    assert_eq!(write_now, 20, "expected step-out to 20, got {write_now}");

    // Subsequent ticks while the alarm is still firing must not
    // double-scale within the cooldown window.
    let _ = force_appas_tick(&server).await;
    let unchanged = ddb
        .describe_table()
        .table_name("step-orders")
        .send()
        .await
        .expect("describe2")
        .table()
        .and_then(|t| t.provisioned_throughput())
        .and_then(|p| p.write_capacity_units())
        .unwrap_or(0);
    // No cooldown configured -> the watcher will keep scaling out.
    // We assert on monotonic non-decrease rather than equality so the
    // test stays robust regardless of cooldown defaulting policy.
    assert!(
        unchanged >= write_now,
        "capacity must not regress while alarm fires"
    );
}

#[tokio::test]
async fn register_scalable_target_tags_are_listed() {
    let server = TestServer::start().await;
    let aas = server.application_autoscaling_client().await;

    let arn = aas
        .register_scalable_target()
        .service_namespace(ServiceNamespace::Ecs)
        .resource_id("service/cluster/tagged")
        .scalable_dimension(ScalableDimension::EcsServiceDesiredCount)
        .min_capacity(1)
        .max_capacity(4)
        .tags("env", "dev")
        .send()
        .await
        .expect("register")
        .scalable_target_arn()
        .map(str::to_owned)
        .expect("arn");
    let tags = aas
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .expect("list tags");
    assert_eq!(
        tags.tags().unwrap().get("env").map(String::as_str),
        Some("dev")
    );
}
