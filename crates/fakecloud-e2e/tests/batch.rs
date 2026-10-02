//! AWS Batch control plane: compute environment, job queue, job definition,
//! scheduling policy, and the job control plane (SubmitJob -> Describe ->
//! Cancel). Real container-backed job execution lands in a later batch.

mod helpers;

use helpers::TestServer;

#[tokio::test]
async fn batch_control_plane_end_to_end() {
    let s = TestServer::start().await;
    let batch = aws_sdk_batch::Client::new(&s.aws_config().await);

    // Compute environment.
    batch
        .create_compute_environment()
        .compute_environment_name("ce1")
        .r#type(aws_sdk_batch::types::CeType::Managed)
        .send()
        .await
        .expect("create CE");
    let ces = batch.describe_compute_environments().send().await.unwrap();
    let ce = ces.compute_environments();
    assert_eq!(ce.len(), 1);
    assert_eq!(ce[0].compute_environment_name(), Some("ce1"));
    assert_eq!(ce[0].status().map(|s| s.as_str()), Some("VALID"));

    // Job queue.
    batch
        .create_job_queue()
        .job_queue_name("q1")
        .priority(1)
        .compute_environment_order(
            aws_sdk_batch::types::ComputeEnvironmentOrder::builder()
                .order(1)
                .compute_environment("ce1")
                .build(),
        )
        .send()
        .await
        .expect("create JQ");

    // Job definition (revisioned).
    let jd = batch
        .register_job_definition()
        .job_definition_name("jd1")
        .r#type(aws_sdk_batch::types::JobDefinitionType::Container)
        .send()
        .await
        .expect("register JD");
    assert_eq!(jd.revision(), Some(1));
    let jd2 = batch
        .register_job_definition()
        .job_definition_name("jd1")
        .r#type(aws_sdk_batch::types::JobDefinitionType::Container)
        .send()
        .await
        .unwrap();
    assert_eq!(jd2.revision(), Some(2));

    // Submit a job: control-plane only, parked at SUBMITTED.
    let job = batch
        .submit_job()
        .job_name("j1")
        .job_queue("q1")
        .job_definition("jd1:1")
        .send()
        .await
        .expect("submit job");
    let job_id = job.job_id().unwrap().to_string();

    let desc = batch.describe_jobs().jobs(&job_id).send().await.unwrap();
    assert_eq!(desc.jobs().len(), 1);
    assert_eq!(
        desc.jobs()[0].status().map(|s| s.as_str()),
        Some("SUBMITTED")
    );

    // Cancel moves it to FAILED.
    batch
        .cancel_job()
        .job_id(&job_id)
        .reason("test cleanup")
        .send()
        .await
        .unwrap();
    let after = batch.describe_jobs().jobs(&job_id).send().await.unwrap();
    assert_eq!(after.jobs()[0].status().map(|s| s.as_str()), Some("FAILED"));
}

#[tokio::test]
async fn batch_consumable_resource_crud_round_trip() {
    let s = TestServer::start().await;
    let batch = aws_sdk_batch::Client::new(&s.aws_config().await);

    let created = batch
        .create_consumable_resource()
        .consumable_resource_name("licenses")
        .total_quantity(10)
        .resource_type("REPLENISHABLE")
        .tags("team", "ml")
        .send()
        .await
        .expect("create consumable resource");
    let arn = created.consumable_resource_arn().unwrap().to_string();
    assert!(arn.ends_with(":consumable-resource/licenses"), "{arn}");

    let d = batch
        .describe_consumable_resource()
        .consumable_resource(&arn)
        .send()
        .await
        .unwrap();
    assert_eq!(d.consumable_resource_name(), Some("licenses"));
    assert_eq!(d.total_quantity(), Some(10));
    assert_eq!(d.in_use_quantity(), Some(0));
    assert_eq!(d.available_quantity(), Some(10));
    assert_eq!(d.resource_type(), Some("REPLENISHABLE"));
    assert_eq!(
        d.tags().and_then(|t| t.get("team")).map(String::as_str),
        Some("ml")
    );

    let u = batch
        .update_consumable_resource()
        .consumable_resource("licenses")
        .operation("ADD")
        .quantity(5)
        .send()
        .await
        .unwrap();
    assert_eq!(u.total_quantity(), Some(15));

    // A job definition that requires the resource; the job shows up in
    // ListJobsByConsumableResource.
    batch
        .create_job_queue()
        .job_queue_name("cq")
        .priority(1)
        .send()
        .await
        .unwrap();
    batch
        .register_job_definition()
        .job_definition_name("lic-jd")
        .r#type(aws_sdk_batch::types::JobDefinitionType::Container)
        .consumable_resource_properties(
            aws_sdk_batch::types::ConsumableResourceProperties::builder()
                .consumable_resource_list(
                    aws_sdk_batch::types::ConsumableResourceRequirement::builder()
                        .consumable_resource("licenses")
                        .quantity(2)
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let job = batch
        .submit_job()
        .job_name("needs-lic")
        .job_queue("cq")
        .job_definition("lic-jd")
        .send()
        .await
        .unwrap();
    let by_res = batch
        .list_jobs_by_consumable_resource()
        .consumable_resource("licenses")
        .send()
        .await
        .unwrap();
    assert_eq!(by_res.jobs().len(), 1);
    assert_eq!(by_res.jobs()[0].job_arn(), job.job_arn());
    assert_eq!(by_res.jobs()[0].quantity(), Some(2));

    let listed = batch
        .list_consumable_resources()
        .filters(
            aws_sdk_batch::types::KeyValuesPair::builder()
                .name("CONSUMABLE_RESOURCE_NAME")
                .values("lic*")
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(listed.consumable_resources().len(), 1);
    assert_eq!(listed.consumable_resources()[0].total_quantity(), Some(15));

    // TagResource / ListTagsForResource work on the new ARN.
    batch
        .tag_resource()
        .resource_arn(&arn)
        .tags("env", "test")
        .send()
        .await
        .unwrap();
    let tags = batch
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .unwrap();
    let tags = tags.tags().unwrap();
    assert_eq!(tags.get("team").map(String::as_str), Some("ml"));
    assert_eq!(tags.get("env").map(String::as_str), Some("test"));

    batch
        .delete_consumable_resource()
        .consumable_resource("licenses")
        .send()
        .await
        .unwrap();
    let err = batch
        .describe_consumable_resource()
        .consumable_resource("licenses")
        .send()
        .await
        .expect_err("deleted resource must not describe");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("ClientException")
    );
}

#[tokio::test]
async fn batch_service_environment_quota_share_and_service_job_round_trip() {
    use aws_sdk_batch::types::{
        CapacityLimit, QuotaShareCapacityLimit, QuotaShareInSharePreemptionState,
        QuotaSharePreemptionConfiguration, QuotaShareResourceSharingConfiguration,
        QuotaShareResourceSharingStrategy, QuotaShareState, ServiceEnvironmentOrder,
        ServiceEnvironmentState, ServiceEnvironmentType, ServiceJobStatus, ServiceJobType,
    };

    let s = TestServer::start().await;
    let batch = aws_sdk_batch::Client::new(&s.aws_config().await);

    // Service environment.
    let env = batch
        .create_service_environment()
        .service_environment_name("sm-env")
        .service_environment_type(ServiceEnvironmentType::SagemakerTraining)
        .capacity_limits(
            CapacityLimit::builder()
                .max_capacity(4)
                .capacity_unit("NUM_INSTANCES")
                .build(),
        )
        .tags("k", "v")
        .send()
        .await
        .expect("create service environment");
    let env_arn = env.service_environment_arn().unwrap().to_string();
    assert!(
        env_arn.ends_with(":service-environment/sm-env"),
        "{env_arn}"
    );
    let envs = batch
        .describe_service_environments()
        .service_environments(&env_arn)
        .send()
        .await
        .unwrap();
    let e = &envs.service_environments()[0];
    assert_eq!(e.service_environment_name(), Some("sm-env"));
    assert_eq!(e.state(), Some(&ServiceEnvironmentState::Enabled));
    assert_eq!(e.capacity_limits()[0].max_capacity(), Some(4));
    assert_eq!(
        e.tags().and_then(|t| t.get("k")).map(String::as_str),
        Some("v")
    );

    // Service job queue backed by the environment.
    let queue = batch
        .create_job_queue()
        .job_queue_name("sm-q")
        .priority(1)
        .service_environment_order(
            ServiceEnvironmentOrder::builder()
                .order(1)
                .service_environment("sm-env")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let queue_arn = queue.job_queue_arn().unwrap().to_string();

    // Quota share on the queue.
    let share = batch
        .create_quota_share()
        .quota_share_name("team-a")
        .job_queue(&queue_arn)
        .capacity_limits(
            QuotaShareCapacityLimit::builder()
                .max_capacity(2)
                .capacity_unit("ml.m5.large")
                .build(),
        )
        .resource_sharing_configuration(
            QuotaShareResourceSharingConfiguration::builder()
                .strategy(QuotaShareResourceSharingStrategy::Lend)
                .build(),
        )
        .preemption_configuration(
            QuotaSharePreemptionConfiguration::builder()
                .in_share_preemption(QuotaShareInSharePreemptionState::Disabled)
                .build(),
        )
        .send()
        .await
        .expect("create quota share");
    let share_arn = share.quota_share_arn().unwrap().to_string();
    let qs = batch
        .describe_quota_share()
        .quota_share_arn(&share_arn)
        .send()
        .await
        .unwrap();
    assert_eq!(qs.quota_share_name(), Some("team-a"));
    assert_eq!(qs.job_queue_arn(), Some(queue_arn.as_str()));
    assert_eq!(qs.state(), Some(&QuotaShareState::Enabled));
    let shares = batch
        .list_quota_shares()
        .job_queue("sm-q")
        .send()
        .await
        .unwrap();
    assert_eq!(shares.quota_shares().len(), 1);

    // Service job: accepted and waiting at RUNNABLE (no SageMaker executor).
    let payload = r#"{"TrainingJobName":"t","ResourceConfig":{"InstanceType":"ml.m5.large","InstanceCount":1,"VolumeSizeInGB":10}}"#;
    let job = batch
        .submit_service_job()
        .job_name("train")
        .job_queue("sm-q")
        .service_job_type(ServiceJobType::SagemakerTraining)
        .service_request_payload(payload)
        .scheduling_priority(3)
        .tags("exp", "1")
        .send()
        .await
        .expect("submit service job");
    let job_id = job.job_id().unwrap().to_string();
    let d = batch
        .describe_service_job()
        .job_id(&job_id)
        .send()
        .await
        .unwrap();
    assert_eq!(d.job_name(), Some("train"));
    assert_eq!(d.job_queue(), Some(queue_arn.as_str()));
    assert_eq!(d.status(), Some(&ServiceJobStatus::Runnable));
    assert_eq!(d.scheduling_priority(), Some(3));
    assert_eq!(d.service_request_payload(), Some(payload));
    assert_eq!(d.is_terminated(), Some(false));
    let tags = batch
        .list_tags_for_resource()
        .resource_arn(job.job_arn().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(
        tags.tags().and_then(|t| t.get("exp")).map(String::as_str),
        Some("1")
    );

    batch
        .update_service_job()
        .job_id(&job_id)
        .scheduling_priority(9)
        .send()
        .await
        .unwrap();
    let listed = batch
        .list_service_jobs()
        .job_queue("sm-q")
        .job_status(ServiceJobStatus::Runnable)
        .send()
        .await
        .unwrap();
    assert_eq!(listed.job_summary_list().len(), 1);
    assert_eq!(listed.job_summary_list()[0].job_id(), Some(job_id.as_str()));

    let snap = batch
        .get_job_queue_snapshot()
        .job_queue("sm-q")
        .send()
        .await
        .unwrap();
    let front = snap.front_of_queue().unwrap().jobs();
    assert_eq!(front.len(), 1);
    assert_eq!(front[0].job_arn(), job.job_arn());

    batch
        .terminate_service_job()
        .job_id(&job_id)
        .reason("done testing")
        .send()
        .await
        .unwrap();
    let d = batch
        .describe_service_job()
        .job_id(&job_id)
        .send()
        .await
        .unwrap();
    assert_eq!(d.status(), Some(&ServiceJobStatus::Failed));
    assert_eq!(d.status_reason(), Some("done testing"));
    assert_eq!(d.is_terminated(), Some(true));
    assert_eq!(d.scheduling_priority(), Some(9));

    // Tear down: share (disable first), queue, environment (disable first).
    batch
        .update_quota_share()
        .quota_share_arn(&share_arn)
        .state(QuotaShareState::Disabled)
        .send()
        .await
        .unwrap();
    batch
        .delete_quota_share()
        .quota_share_arn(&share_arn)
        .send()
        .await
        .unwrap();
    batch
        .delete_job_queue()
        .job_queue("sm-q")
        .send()
        .await
        .unwrap();
    batch
        .update_service_environment()
        .service_environment("sm-env")
        .state(ServiceEnvironmentState::Disabled)
        .send()
        .await
        .unwrap();
    batch
        .delete_service_environment()
        .service_environment(&env_arn)
        .send()
        .await
        .unwrap();
    let envs = batch.describe_service_environments().send().await.unwrap();
    assert!(envs.service_environments().is_empty());
}
