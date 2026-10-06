//! Reset endpoints in persistent mode (#2694): a reset must write the cleared
//! state through to disk, so a restart against the same data directory does
//! not bring the reset resources back.

mod helpers;

use std::path::Path;

use aws_credential_types::Credentials;
use aws_sdk_elasticloadbalancingv2::types::TargetTypeEnum;
use fakecloud_sdk::types::PutServiceQuotaRequest;
use fakecloud_sdk::FakeCloud;
use helpers::TestServer;

const OTHER_ACCOUNT: &str = "222222222222";
const VPC: &str = "vpc";
const SGS_PER_ENI: &str = "L-2AFB9258";
const SGS_PER_ENI_DEFAULT: f64 = 5.0;

async fn start(data_path: &Path) -> TestServer {
    TestServer::start_full(
        &[("FAKECLOUD_CONTAINER_CLI", "false")],
        &[
            "--storage-mode",
            "persistent",
            "--data-path",
            &data_path.display().to_string(),
        ],
    )
    .await
}

async fn config_with(server: &TestServer, akid: &str, secret: &str) -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "reset-e2e"))
        .load()
        .await
}

async fn queue_urls(sqs: &aws_sdk_sqs::Client) -> Vec<String> {
    sqs.list_queues()
        .send()
        .await
        .unwrap()
        .queue_urls()
        .to_vec()
}

async fn bucket_exists(s3: &aws_sdk_s3::Client, bucket: &str) -> bool {
    match s3.head_bucket().bucket(bucket).send().await {
        Ok(_) => true,
        Err(err) => {
            let status = err.raw_response().map(|r| r.status().as_u16());
            assert_eq!(status, Some(404), "unexpected HeadBucket error: {err:?}");
            false
        }
    }
}

async fn table_names(ddb: &aws_sdk_dynamodb::Client) -> Vec<String> {
    ddb.list_tables()
        .send()
        .await
        .unwrap()
        .table_names()
        .to_vec()
}

async fn role_exists(iam: &aws_sdk_iam::Client, role: &str) -> bool {
    iam.get_role().role_name(role).send().await.is_ok()
}

async fn parameter_exists(ssm: &aws_sdk_ssm::Client, name: &str) -> bool {
    ssm.get_parameter().name(name).send().await.is_ok()
}

async fn applied_sgs_per_eni(fc: &FakeCloud) -> f64 {
    fc.service_quotas()
        .get_quotas(None, None, Some(VPC))
        .await
        .unwrap()
        .quotas
        .into_iter()
        .find(|q| q.quota_code == SGS_PER_ENI)
        .expect("quota listed")
        .applied_value
}

async fn swf_domain_exists(swf: &aws_sdk_swf::Client, name: &str) -> bool {
    swf.describe_domain().name(name).send().await.is_ok()
}

async fn target_group_names(elb: &aws_sdk_elasticloadbalancingv2::Client) -> Vec<String> {
    elb.describe_target_groups()
        .send()
        .await
        .unwrap()
        .target_groups()
        .iter()
        .filter_map(|tg| tg.target_group_name().map(str::to_string))
        .collect()
}

const ROLE_TRUST: &str = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#;

/// Create one resource in each service the test covers.
async fn create_everything(server: &TestServer, fc: &FakeCloud) {
    let config = server.aws_config().await;
    aws_sdk_sqs::Client::new(&config)
        .create_queue()
        .queue_name("reset-queue")
        .send()
        .await
        .unwrap();
    let s3 = aws_sdk_s3::Client::new(&config);
    s3.create_bucket()
        .bucket("reset-bucket")
        .send()
        .await
        .unwrap();
    s3.put_object()
        .bucket("reset-bucket")
        .key("k")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"data"))
        .send()
        .await
        .unwrap();
    aws_sdk_dynamodb::Client::new(&config)
        .create_table()
        .table_name("reset-table")
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("pk")
                .attribute_type(aws_sdk_dynamodb::types::ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .key_schema(
            aws_sdk_dynamodb::types::KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(aws_sdk_dynamodb::types::KeyType::Hash)
                .build()
                .unwrap(),
        )
        .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();
    aws_sdk_iam::Client::new(&config)
        .create_role()
        .role_name("reset-role")
        .assume_role_policy_document(ROLE_TRUST)
        .send()
        .await
        .unwrap();
    aws_sdk_ssm::Client::new(&config)
        .put_parameter()
        .name("/reset/param")
        .value("v")
        .r#type(aws_sdk_ssm::types::ParameterType::String)
        .send()
        .await
        .unwrap();
    fc.service_quotas()
        .put_quota(
            VPC,
            SGS_PER_ENI,
            &PutServiceQuotaRequest {
                value: Some(2.0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    aws_sdk_swf::Client::new(&config)
        .register_domain()
        .name("reset-domain")
        .workflow_execution_retention_period_in_days("1")
        .send()
        .await
        .unwrap();
    aws_sdk_elasticloadbalancingv2::Client::new(&config)
        .create_target_group()
        .name("reset-tg")
        .target_type(TargetTypeEnum::Lambda)
        .send()
        .await
        .unwrap();
}

/// Assert every resource [`create_everything`] made is gone.
async fn assert_everything_gone(server: &TestServer, fc: &FakeCloud) {
    let config = server.aws_config().await;
    assert!(queue_urls(&aws_sdk_sqs::Client::new(&config))
        .await
        .is_empty());
    assert!(!bucket_exists(&aws_sdk_s3::Client::new(&config), "reset-bucket").await);
    assert!(table_names(&aws_sdk_dynamodb::Client::new(&config))
        .await
        .is_empty());
    assert!(!role_exists(&aws_sdk_iam::Client::new(&config), "reset-role").await);
    assert!(!parameter_exists(&aws_sdk_ssm::Client::new(&config), "/reset/param").await);
    assert_eq!(applied_sgs_per_eni(fc).await, SGS_PER_ENI_DEFAULT);
    assert!(!swf_domain_exists(&aws_sdk_swf::Client::new(&config), "reset-domain").await);
    assert!(
        target_group_names(&aws_sdk_elasticloadbalancingv2::Client::new(&config))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn full_reset_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = start(tmp.path()).await;
    let fc = FakeCloud::new(server.endpoint());

    create_everything(&server, &fc).await;
    // Sanity: without a reset everything comes back after a restart.
    server.restart().await;
    let fc = FakeCloud::new(server.endpoint());
    let config = server.aws_config().await;
    assert_eq!(
        queue_urls(&aws_sdk_sqs::Client::new(&config)).await.len(),
        1
    );
    assert!(bucket_exists(&aws_sdk_s3::Client::new(&config), "reset-bucket").await);
    assert_eq!(applied_sgs_per_eni(&fc).await, 2.0);
    assert!(swf_domain_exists(&aws_sdk_swf::Client::new(&config), "reset-domain").await);

    fc.reset().await.unwrap();
    assert_everything_gone(&server, &fc).await;

    server.restart().await;
    let fc = FakeCloud::new(server.endpoint());
    assert_everything_gone(&server, &fc).await;

    // The reset names stay free: recreating them works after the restart.
    create_everything(&server, &fc).await;
}

#[tokio::test]
async fn per_service_reset_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = start(tmp.path()).await;
    let fc = FakeCloud::new(server.endpoint());
    create_everything(&server, &fc).await;

    for service in ["sqs", "s3", "servicequotas", "swf", "elbv2"] {
        fc.reset_service(service).await.unwrap();
    }

    server.restart().await;
    let fc = FakeCloud::new(server.endpoint());
    let config = server.aws_config().await;
    // The reset services stay reset...
    assert!(queue_urls(&aws_sdk_sqs::Client::new(&config))
        .await
        .is_empty());
    assert!(!bucket_exists(&aws_sdk_s3::Client::new(&config), "reset-bucket").await);
    assert_eq!(applied_sgs_per_eni(&fc).await, SGS_PER_ENI_DEFAULT);
    assert!(!swf_domain_exists(&aws_sdk_swf::Client::new(&config), "reset-domain").await);
    assert!(
        target_group_names(&aws_sdk_elasticloadbalancingv2::Client::new(&config))
            .await
            .is_empty()
    );
    // ...and the others keep their state.
    assert_eq!(
        table_names(&aws_sdk_dynamodb::Client::new(&config)).await,
        vec!["reset-table".to_string()]
    );
    assert!(role_exists(&aws_sdk_iam::Client::new(&config), "reset-role").await);
    assert!(parameter_exists(&aws_sdk_ssm::Client::new(&config), "/reset/param").await);
}

#[tokio::test]
async fn per_account_reset_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = start(tmp.path()).await;
    let fc = FakeCloud::new(server.endpoint());
    let (akid, secret) = server.create_admin(OTHER_ACCOUNT, "admin-b").await;

    let own = server.aws_config().await;
    let other = config_with(&server, &akid, &secret).await;
    for config in [&own, &other] {
        aws_sdk_sqs::Client::new(config)
            .create_queue()
            .queue_name("per-account")
            .send()
            .await
            .unwrap();
        aws_sdk_swf::Client::new(config)
            .register_domain()
            .name("per-account")
            .workflow_execution_retention_period_in_days("1")
            .send()
            .await
            .unwrap();
    }
    aws_sdk_s3::Client::new(&other)
        .create_bucket()
        .bucket("per-account-other")
        .send()
        .await
        .unwrap();

    // The other account's credentials route to that account.
    let other_queues = queue_urls(&aws_sdk_sqs::Client::new(&other)).await;
    assert_eq!(other_queues.len(), 1, "{other_queues:?}");
    assert!(other_queues[0].contains(OTHER_ACCOUNT), "{other_queues:?}");

    for service in ["sqs", "swf", "s3"] {
        fc.reset_service_for_account(service, OTHER_ACCOUNT)
            .await
            .unwrap();
    }

    server.restart().await;
    let own = server.aws_config().await;
    let other = config_with(&server, &akid, &secret).await;
    // The admin create-admin bootstrapped survives the restart, so its
    // credentials still route to the other account.
    let identity = aws_sdk_sts::Client::new(&other)
        .get_caller_identity()
        .send()
        .await
        .unwrap();
    assert_eq!(identity.account(), Some(OTHER_ACCOUNT));
    // The other account's resources stay gone...
    assert!(queue_urls(&aws_sdk_sqs::Client::new(&other))
        .await
        .is_empty());
    assert!(!swf_domain_exists(&aws_sdk_swf::Client::new(&other), "per-account").await);
    assert!(!bucket_exists(&aws_sdk_s3::Client::new(&own), "per-account-other").await);
    // ...while the default account's survive.
    let own_queues = queue_urls(&aws_sdk_sqs::Client::new(&own)).await;
    assert_eq!(own_queues.len(), 1, "{own_queues:?}");
    assert!(own_queues[0].contains("123456789012"), "{own_queues:?}");
    assert!(swf_domain_exists(&aws_sdk_swf::Client::new(&own), "per-account").await);
}
