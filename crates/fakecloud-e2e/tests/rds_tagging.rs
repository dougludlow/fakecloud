//! End-to-end coverage for RDS tagging multiplexed across every
//! supported resource type (M4 batch).
//!
//! `AddTagsToResource`, `ListTagsForResource`, and
//! `RemoveTagsFromResource` accept any RDS resource ARN and dispatch
//! on the resource-type segment (`db`, `snapshot`, `cluster`,
//! `cluster-snapshot`, `pg`, `cluster-pg`, `og`, `subgrp`, `secgrp`,
//! `db-proxy`, `es`). These tests exercise the dispatch over the
//! types that are easy to spin up via the SDK without booting a real
//! engine container, plus the unknown-resource-type error path.

mod helpers;

use aws_sdk_rds::types::{Tag, UserAuthConfig};
use helpers::TestServer;

async fn add_tag(client: &aws_sdk_rds::Client, arn: &str, key: &str, value: &str) {
    client
        .add_tags_to_resource()
        .resource_name(arn)
        .tags(Tag::builder().key(key).value(value).build())
        .send()
        .await
        .unwrap_or_else(|e| panic!("AddTagsToResource failed for {arn}: {e}"));
}

async fn list_tag_keys(client: &aws_sdk_rds::Client, arn: &str) -> Vec<String> {
    let resp = client
        .list_tags_for_resource()
        .resource_name(arn)
        .send()
        .await
        .unwrap_or_else(|e| panic!("ListTagsForResource failed for {arn}: {e}"));
    resp.tag_list()
        .iter()
        .filter_map(|t| t.key().map(str::to_string))
        .collect()
}

async fn remove_tag(client: &aws_sdk_rds::Client, arn: &str, key: &str) {
    client
        .remove_tags_from_resource()
        .resource_name(arn)
        .tag_keys(key)
        .send()
        .await
        .unwrap_or_else(|e| panic!("RemoveTagsFromResource failed for {arn}: {e}"));
}

async fn assert_tag_round_trip(client: &aws_sdk_rds::Client, arn: &str, label: &str) {
    add_tag(client, arn, "env", "prod").await;
    add_tag(client, arn, "team", "platform").await;

    let keys = list_tag_keys(client, arn).await;
    assert!(
        keys.contains(&"env".to_string()) && keys.contains(&"team".to_string()),
        "[{label}] expected env+team after AddTags, got {keys:?}"
    );

    remove_tag(client, arn, "env").await;

    let keys = list_tag_keys(client, arn).await;
    assert!(
        !keys.contains(&"env".to_string()) && keys.contains(&"team".to_string()),
        "[{label}] expected env removed and team present after RemoveTags, got {keys:?}"
    );
}

#[tokio::test]
async fn rds_tagging_db_parameter_group() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let arn = client
        .create_db_parameter_group()
        .db_parameter_group_name("tag-pg")
        .db_parameter_group_family("postgres16")
        .description("tag dispatch test")
        .send()
        .await
        .unwrap()
        .db_parameter_group()
        .and_then(|g| g.db_parameter_group_arn())
        .map(str::to_string)
        .expect("parameter group arn");
    assert!(arn.contains(":pg:"), "expected pg ARN segment, got {arn}");

    assert_tag_round_trip(&client, &arn, "pg").await;
}

#[tokio::test]
async fn rds_tagging_db_subnet_group() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let arn = client
        .create_db_subnet_group()
        .db_subnet_group_name("tag-subnet")
        .db_subnet_group_description("tag dispatch test")
        .subnet_ids("subnet-aaa")
        .subnet_ids("subnet-bbb")
        .send()
        .await
        .unwrap()
        .db_subnet_group()
        .and_then(|g| g.db_subnet_group_arn())
        .map(str::to_string)
        .expect("subnet group arn");
    assert!(
        arn.contains(":subgrp:"),
        "expected subgrp ARN segment, got {arn}"
    );

    assert_tag_round_trip(&client, &arn, "subgrp").await;
}

#[tokio::test]
async fn rds_tagging_db_cluster() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let arn = client
        .create_db_cluster()
        .db_cluster_identifier("tag-cluster")
        .engine("aurora-postgresql")
        .send()
        .await
        .unwrap()
        .db_cluster()
        .and_then(|c| c.db_cluster_arn())
        .map(str::to_string)
        .expect("cluster arn");
    assert!(
        arn.contains(":cluster:"),
        "expected cluster ARN segment, got {arn}"
    );

    assert_tag_round_trip(&client, &arn, "cluster").await;
}

#[tokio::test]
async fn rds_tagging_db_cluster_snapshot() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    client
        .create_db_cluster()
        .db_cluster_identifier("tag-cluster-for-snap")
        .engine("aurora-postgresql")
        .send()
        .await
        .unwrap();

    let arn = client
        .create_db_cluster_snapshot()
        .db_cluster_identifier("tag-cluster-for-snap")
        .db_cluster_snapshot_identifier("tag-csnap")
        .send()
        .await
        .unwrap()
        .db_cluster_snapshot()
        .and_then(|s| s.db_cluster_snapshot_arn())
        .map(str::to_string)
        .expect("cluster snapshot arn");
    assert!(
        arn.contains(":cluster-snapshot:"),
        "expected cluster-snapshot ARN segment, got {arn}"
    );

    assert_tag_round_trip(&client, &arn, "cluster-snapshot").await;
}

#[tokio::test]
async fn rds_tagging_db_cluster_parameter_group() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let arn = client
        .create_db_cluster_parameter_group()
        .db_cluster_parameter_group_name("tag-cpg")
        .db_parameter_group_family("aurora-postgresql15")
        .description("tag dispatch test")
        .send()
        .await
        .unwrap()
        .db_cluster_parameter_group()
        .and_then(|g| g.db_cluster_parameter_group_arn())
        .map(str::to_string)
        .expect("cluster parameter group arn");
    assert!(
        arn.contains(":cluster-pg:"),
        "expected cluster-pg ARN segment, got {arn}"
    );

    assert_tag_round_trip(&client, &arn, "cluster-pg").await;
}

#[tokio::test]
async fn rds_tagging_option_group() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    // CreateOptionGroup currently emits a flat XML body without an
    // <OptionGroup> wrapper, so the SDK can't extract the arn from the
    // response; we construct it deterministically from the testkit's
    // hardcoded account/region. The dispatcher itself is what we're
    // testing, not the response shape.
    client
        .create_option_group()
        .option_group_name("tag-og")
        .engine_name("mysql")
        .major_engine_version("8.0")
        .option_group_description("tag dispatch test")
        .send()
        .await
        .unwrap();

    let arn = "arn:aws:rds:us-east-1:123456789012:og:tag-og";
    assert_tag_round_trip(&client, arn, "og").await;
}

#[tokio::test]
async fn rds_tagging_db_proxy() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    // The ARN is built deterministically; the dispatcher under test
    // resolves it to the `proxies` extras bucket.
    client
        .create_db_proxy()
        .db_proxy_name("tag-proxy")
        .engine_family(aws_sdk_rds::types::EngineFamily::Postgresql)
        .auth(
            UserAuthConfig::builder()
                .auth_scheme(aws_sdk_rds::types::AuthScheme::Secrets)
                .secret_arn("arn:aws:secretsmanager:us-east-1:123:secret:dummy")
                .build(),
        )
        .role_arn("arn:aws:iam::123:role/dummy")
        .vpc_subnet_ids("subnet-aaa")
        .vpc_subnet_ids("subnet-bbb")
        .send()
        .await
        .unwrap();

    let arn = "arn:aws:rds:us-east-1:123456789012:db-proxy:tag-proxy";
    assert_tag_round_trip(&client, arn, "db-proxy").await;
}

#[tokio::test]
async fn rds_tagging_event_subscription() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    // Same shape gap as option_group / db-proxy — construct the ARN
    // ourselves so the test exercises the `es` dispatch arm.
    client
        .create_event_subscription()
        .subscription_name("tag-es")
        .sns_topic_arn("arn:aws:sns:us-east-1:123456789012:dummy")
        .source_type("db-instance")
        .send()
        .await
        .unwrap();

    let arn = "arn:aws:rds:us-east-1:123456789012:es:tag-es";
    assert_tag_round_trip(&client, arn, "es").await;
}

// `db` ARN coverage already lives in `rds.rs::rds_tag_roundtrip`,
// which exercises the same dispatcher arm against a real DBInstance.
// Replicating it here would just double the Docker engine startup
// cost without adding signal.

#[tokio::test]
async fn rds_tagging_unknown_arn_segment_errors() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    // Smithy declares every per-resource `*NotFoundFault` shape on the
    // tag ops but no generic "bad ARN" code, so the unknown-segment
    // case falls back to `DBInstanceNotFound` — see
    // `tag_resource_not_found` in `fakecloud-rds`.
    let err = client
        .list_tags_for_resource()
        .resource_name("arn:aws:rds:us-east-1:000000000000:bogus:nope")
        .send()
        .await
        .expect_err("unknown segment should error");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("DBInstanceNotFound"),
        "unknown ARN segment falls back to declared DBInstanceNotFound"
    );
}

#[tokio::test]
async fn rds_tagging_missing_resource_returns_typed_not_found() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    // Well-formed ARN, recognised segment, missing instance: the typed
    // NotFound is what AWS clients (and most IaC tooling) expect.
    let err = client
        .list_tags_for_resource()
        .resource_name("arn:aws:rds:us-east-1:000000000000:db:does-not-exist")
        .send()
        .await
        .expect_err("missing db should error");
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("DBInstanceNotFound"),
        "missing DB should map to DBInstanceNotFound"
    );
}

fn tag(key: &str, value: &str) -> Tag {
    Tag::builder().key(key).value(value).build()
}

fn tag_keys(tags: &[Tag]) -> Vec<String> {
    tags.iter()
        .filter_map(|t| t.key().map(str::to_string))
        .collect()
}

/// Tags named on CreateDBCluster are stored, not dropped: the
/// DescribeDBClusters TagList and ListTagsForResource both report them.
#[tokio::test]
async fn rds_create_db_cluster_keeps_request_tags() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let created = client
        .create_db_cluster()
        .db_cluster_identifier("tagged-cluster")
        .engine("aurora-postgresql")
        .tags(tag("env", "prod"))
        .send()
        .await
        .unwrap();
    let cluster = created.db_cluster().expect("cluster");
    assert_eq!(tag_keys(cluster.tag_list()), vec!["env"]);
    let arn = cluster.db_cluster_arn().unwrap().to_string();

    let described = client
        .describe_db_clusters()
        .db_cluster_identifier("tagged-cluster")
        .send()
        .await
        .unwrap();
    assert_eq!(tag_keys(described.db_clusters()[0].tag_list()), vec!["env"]);
    assert_eq!(list_tag_keys(&client, &arn).await, vec!["env"]);
}

/// Cluster snapshots and the clusters restored from them take the tags
/// named on the request; a copy carries the source's tags only with
/// CopyTags, and request tags win over CopyTags.
#[tokio::test]
async fn rds_cluster_snapshot_copy_and_restore_tags() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    client
        .create_db_cluster()
        .db_cluster_identifier("snap-src-cluster")
        .engine("aurora-postgresql")
        .tags(tag("cluster", "c"))
        .send()
        .await
        .unwrap();

    let snapshot = client
        .create_db_cluster_snapshot()
        .db_cluster_identifier("snap-src-cluster")
        .db_cluster_snapshot_identifier("tagged-csnap")
        .tags(tag("snap", "s"))
        .send()
        .await
        .unwrap();
    let snapshot = snapshot.db_cluster_snapshot().expect("snapshot");
    assert_eq!(tag_keys(snapshot.tag_list()), vec!["snap"]);
    let snapshot_arn = snapshot.db_cluster_snapshot_arn().unwrap().to_string();
    assert_eq!(list_tag_keys(&client, &snapshot_arn).await, vec!["snap"]);

    // The cluster has no CopyTagsToSnapshot, so an untagged snapshot of
    // it starts untagged.
    let bare = client
        .create_db_cluster_snapshot()
        .db_cluster_identifier("snap-src-cluster")
        .db_cluster_snapshot_identifier("bare-csnap")
        .send()
        .await
        .unwrap();
    assert!(bare.db_cluster_snapshot().unwrap().tag_list().is_empty());

    for (target, copy_tags, request_tag, expected) in [
        ("copy-plain", false, None, vec![]),
        ("copy-copied", true, None, vec!["snap"]),
        ("copy-named", true, Some(tag("copy", "x")), vec!["copy"]),
    ] {
        let mut call = client
            .copy_db_cluster_snapshot()
            .source_db_cluster_snapshot_identifier("tagged-csnap")
            .target_db_cluster_snapshot_identifier(target)
            .copy_tags(copy_tags);
        if let Some(t) = request_tag {
            call = call.tags(t);
        }
        let copied = call.send().await.unwrap();
        let arn = copied
            .db_cluster_snapshot()
            .and_then(|s| s.db_cluster_snapshot_arn())
            .unwrap()
            .to_string();
        assert_eq!(list_tag_keys(&client, &arn).await, expected, "[{target}]");
    }

    let restored = client
        .restore_db_cluster_from_snapshot()
        .db_cluster_identifier("restored-tagged")
        .snapshot_identifier("tagged-csnap")
        .engine("aurora-postgresql")
        .tags(tag("restored", "r"))
        .send()
        .await
        .unwrap();
    let arn = restored
        .db_cluster()
        .and_then(|c| c.db_cluster_arn())
        .unwrap()
        .to_string();
    assert_eq!(list_tag_keys(&client, &arn).await, vec!["restored"]);

    let pitr = client
        .restore_db_cluster_to_point_in_time()
        .db_cluster_identifier("pitr-tagged")
        .source_db_cluster_identifier("snap-src-cluster")
        .use_latest_restorable_time(true)
        .tags(tag("pitr", "p"))
        .send()
        .await
        .unwrap();
    let arn = pitr
        .db_cluster()
        .and_then(|c| c.db_cluster_arn())
        .unwrap()
        .to_string();
    assert_eq!(list_tag_keys(&client, &arn).await, vec!["pitr"]);
}

/// CreateDBClusterParameterGroup keeps its tags, and the copy keeps the
/// source's family and parameters with the request's own description
/// and tags.
#[tokio::test]
async fn rds_cluster_parameter_group_create_and_copy_keep_state() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let arn = client
        .create_db_cluster_parameter_group()
        .db_cluster_parameter_group_name("cpg-src")
        .db_parameter_group_family("aurora-mysql8.0")
        .description("source")
        .tags(tag("env", "prod"))
        .send()
        .await
        .unwrap()
        .db_cluster_parameter_group()
        .and_then(|g| g.db_cluster_parameter_group_arn())
        .map(str::to_string)
        .unwrap();
    assert_eq!(list_tag_keys(&client, &arn).await, vec!["env"]);

    let copy = client
        .copy_db_cluster_parameter_group()
        .source_db_cluster_parameter_group_identifier("cpg-src")
        .target_db_cluster_parameter_group_identifier("cpg-dst")
        .target_db_cluster_parameter_group_description("the copy")
        .tags(tag("team", "data"))
        .send()
        .await
        .unwrap();
    let group = copy.db_cluster_parameter_group().unwrap();
    assert_eq!(group.db_parameter_group_family(), Some("aurora-mysql8.0"));
    assert_eq!(group.description(), Some("the copy"));
    let dst_arn = group.db_cluster_parameter_group_arn().unwrap().to_string();
    assert_eq!(list_tag_keys(&client, &dst_arn).await, vec!["team"]);

    let described = client
        .describe_db_cluster_parameter_groups()
        .db_cluster_parameter_group_name("cpg-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(
        described.db_cluster_parameter_groups()[0].db_parameter_group_family(),
        Some("aurora-mysql8.0")
    );
}

/// CopyDBParameterGroup tags the copy with the request's tags.
#[tokio::test]
async fn rds_copy_db_parameter_group_keeps_request_tags() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    client
        .create_db_parameter_group()
        .db_parameter_group_name("pg-src")
        .db_parameter_group_family("postgres16")
        .description("source")
        .tags(tag("env", "prod"))
        .send()
        .await
        .unwrap();
    let arn = client
        .copy_db_parameter_group()
        .source_db_parameter_group_identifier("pg-src")
        .target_db_parameter_group_identifier("pg-dst")
        .target_db_parameter_group_description("copy")
        .tags(tag("team", "data"))
        .send()
        .await
        .unwrap()
        .db_parameter_group()
        .and_then(|g| g.db_parameter_group_arn())
        .map(str::to_string)
        .unwrap();
    assert_eq!(list_tag_keys(&client, &arn).await, vec!["team"]);
}

/// CreateDBProxy / CreateDBProxyEndpoint keep what the request set:
/// auth, role, subnets, security groups, settings and tags.
#[tokio::test]
async fn rds_create_db_proxy_and_endpoint_keep_request_settings() {
    let server = TestServer::start().await;
    let client = server.rds_client().await;

    let created = client
        .create_db_proxy()
        .db_proxy_name("full-proxy")
        .engine_family(aws_sdk_rds::types::EngineFamily::Mysql)
        .auth(
            UserAuthConfig::builder()
                .auth_scheme(aws_sdk_rds::types::AuthScheme::Secrets)
                .secret_arn("arn:aws:secretsmanager:us-east-1:123456789012:secret:db")
                .iam_auth(aws_sdk_rds::types::IamAuthMode::Disabled)
                .build(),
        )
        .role_arn("arn:aws:iam::123456789012:role/proxy")
        .vpc_subnet_ids("subnet-aaa")
        .vpc_subnet_ids("subnet-bbb")
        .vpc_security_group_ids("sg-111")
        .require_tls(true)
        .idle_client_timeout(900)
        .tags(tag("env", "prod"))
        .send()
        .await
        .unwrap();
    let proxy = created.db_proxy().expect("CreateDBProxy returns the proxy");
    assert_eq!(proxy.db_proxy_name(), Some("full-proxy"));
    let proxy_arn = proxy.db_proxy_arn().unwrap().to_string();

    let described = client.describe_db_proxies().send().await.unwrap();
    let proxy = described
        .db_proxies()
        .iter()
        .find(|p| p.db_proxy_name() == Some("full-proxy"))
        .expect("proxy listed");
    assert_eq!(
        proxy.role_arn(),
        Some("arn:aws:iam::123456789012:role/proxy")
    );
    assert_eq!(proxy.vpc_subnet_ids(), ["subnet-aaa", "subnet-bbb"]);
    assert_eq!(proxy.vpc_security_group_ids(), ["sg-111"]);
    assert_eq!(proxy.require_tls(), Some(true));
    assert_eq!(proxy.idle_client_timeout(), Some(900));
    assert!(proxy.endpoint().is_some());
    assert_eq!(proxy.auth().len(), 1);
    assert_eq!(
        proxy.auth()[0].auth_scheme(),
        Some(&aws_sdk_rds::types::AuthScheme::Secrets)
    );
    assert_eq!(
        proxy.auth()[0].secret_arn(),
        Some("arn:aws:secretsmanager:us-east-1:123456789012:secret:db")
    );
    assert_eq!(list_tag_keys(&client, &proxy_arn).await, vec!["env"]);

    let endpoint = client
        .create_db_proxy_endpoint()
        .db_proxy_name("full-proxy")
        .db_proxy_endpoint_name("full-proxy-ro")
        .vpc_subnet_ids("subnet-aaa")
        .target_role(aws_sdk_rds::types::DbProxyEndpointTargetRole::ReadOnly)
        .tags(tag("tier", "read"))
        .send()
        .await
        .unwrap();
    let endpoint = endpoint.db_proxy_endpoint().expect("endpoint");
    let endpoint_arn = endpoint.db_proxy_endpoint_arn().unwrap().to_string();

    let described = client.describe_db_proxy_endpoints().send().await.unwrap();
    let endpoint = described
        .db_proxy_endpoints()
        .iter()
        .find(|e| e.db_proxy_endpoint_name() == Some("full-proxy-ro"))
        .expect("endpoint listed");
    assert_eq!(endpoint.db_proxy_name(), Some("full-proxy"));
    assert_eq!(endpoint.vpc_subnet_ids(), ["subnet-aaa"]);
    assert_eq!(
        endpoint.target_role(),
        Some(&aws_sdk_rds::types::DbProxyEndpointTargetRole::ReadOnly)
    );
    assert_eq!(endpoint.is_default(), Some(false));
    assert_eq!(list_tag_keys(&client, &endpoint_arn).await, vec!["tier"]);
}
