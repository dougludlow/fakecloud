mod helpers;

use helpers::TestServer;

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn require_docker_or_skip(test: &str) -> bool {
    if docker_available() {
        return true;
    }
    if std::env::var("CI").is_ok() {
        panic!("docker is required for {test} in CI");
    }
    eprintln!("Skipping {test}: docker not available");
    false
}

/// Cache cluster endpoint survives a restart. Pre-fix, the cluster row
/// was persisted with `cache_cluster_status=available` but the Docker
/// container was gone, so the endpoint TCP-port wouldn't accept
/// connections after restart. Same bug class as RDS #1338.
#[tokio::test]
async fn persistence_cache_cluster_endpoint_works_after_restart() {
    if !require_docker_or_skip("persistence_cache_cluster_endpoint_works_after_restart") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    // Drop the cluster's durable data volume even if the test fails.
    let _volumes = helpers::DataVolumeGuard::new(tmp.path());
    let mut server = TestServer::start_full(&[], &extra_args).await;
    let client = server.elasticache_client().await;

    client
        .create_cache_cluster()
        .cache_cluster_id("restart-cache")
        .cache_node_type("cache.t3.micro")
        .preferred_availability_zone("us-east-1a")
        .send()
        .await
        .unwrap();

    // The backing container starts in the background; let the cluster reach
    // "available" before snapshotting so the restart recovers a complete row
    // (bug-audit 2026-05-28, 3.2).
    helpers::wait_for_cache_cluster_available(&client, "restart-cache", 120).await;

    drop(client);
    server.restart().await;
    let client = server.elasticache_client().await;

    // Poll status back to available after recovery.
    let mut status_ok = false;
    let mut port = 0;
    for _ in 0..60 {
        let resp = client
            .describe_cache_clusters()
            .cache_cluster_id("restart-cache")
            .show_cache_node_info(true)
            .send()
            .await
            .unwrap();
        let clusters = resp.cache_clusters();
        if let Some(cluster) = clusters.first() {
            if cluster.cache_cluster_status() == Some("available") {
                let endpoint = cluster.cache_nodes()[0]
                    .endpoint()
                    .expect("cache node endpoint");
                port = endpoint.port().expect("port");
                status_ok = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(status_ok, "cache cluster did not recover to `available`");
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await;
    assert!(
        stream.is_ok(),
        "recovered cache cluster endpoint must accept connections: {stream:?}",
    );
}

/// Send one RESP command to a Redis endpoint and return the raw reply text.
async fn redis_cmd(stream: &mut tokio::net::TcpStream, cmd: &[u8]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(cmd).await.expect("write redis command");
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).await.expect("read redis reply");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Poll DescribeCacheClusters until the cluster is `available`, returning the
/// node endpoint port. Panics if it never recovers.
async fn wait_cache_port(client: &aws_sdk_elasticache::Client, id: &str) -> i32 {
    for _ in 0..120 {
        let resp = client
            .describe_cache_clusters()
            .cache_cluster_id(id)
            .show_cache_node_info(true)
            .send()
            .await
            .unwrap();
        if let Some(cluster) = resp.cache_clusters().first() {
            if cluster.cache_cluster_status() == Some("available") {
                if let Some(port) = cluster
                    .cache_nodes()
                    .first()
                    .and_then(|n| n.endpoint())
                    .and_then(|e| e.port())
                {
                    return port;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    panic!("cache cluster {id} never reached available with an endpoint port");
}

/// Data WRITTEN to a Redis cache survives a restart, not just the endpoint.
/// ElastiCache backs Redis/Valkey with a durable `/data` volume, so a key SET
/// (and SAVEd to the RDB) before restart is still readable after the backing
/// container is recreated. The explicit SAVE forces the RDB to the volume so
/// the abrupt container teardown on restart can't lose the unflushed write.
#[tokio::test]
async fn persistence_cache_data_survives_restart() {
    if !require_docker_or_skip("persistence_cache_data_survives_restart") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let extra_args = ["--storage-mode", "persistent", "--data-path", &data_path];
    // Drop the cluster's durable data volume even if the test fails.
    let volume_guard = helpers::DataVolumeGuard::new(tmp.path());
    let mut server = TestServer::start_full(&[], &extra_args).await;
    let client = server.elasticache_client().await;

    client
        .create_cache_cluster()
        .cache_cluster_id("data-cache")
        .engine("redis")
        .cache_node_type("cache.t3.micro")
        .preferred_availability_zone("us-east-1a")
        .send()
        .await
        .unwrap();
    helpers::wait_for_cache_cluster_available(&client, "data-cache", 120).await;
    let port = wait_cache_port(&client, "data-cache").await;

    // SET a key, then SAVE so the RDB hits the durable /data volume.
    {
        let mut s = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .expect("connect redis to seed");
        let set = redis_cmd(
            &mut s,
            b"*3\r\n$3\r\nSET\r\n$11\r\ndurable-key\r\n$13\r\nsurvive-value\r\n",
        )
        .await;
        assert!(set.starts_with("+OK"), "SET should succeed: {set:?}");
        let save = redis_cmd(&mut s, b"*1\r\n$4\r\nSAVE\r\n").await;
        assert!(save.starts_with("+OK"), "SAVE should succeed: {save:?}");
    }

    drop(client);
    server.restart().await;
    let client = server.elasticache_client().await;

    let port = wait_cache_port(&client, "data-cache").await;

    // The recovered container reloads the RDB from the volume; retry GET while
    // redis finishes loading.
    let mut value = String::new();
    for _ in 0..40 {
        if let Ok(mut s) = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await {
            value = redis_cmd(&mut s, b"*2\r\n$3\r\nGET\r\n$11\r\ndurable-key\r\n").await;
            if value.contains("survive-value") {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        value.contains("survive-value"),
        "cached key must survive the restart via the durable /data volume, got {value:?}"
    );

    // The volume is scoped to this data dir (#2630), and DeleteCacheCluster
    // drops it so nothing is left for a later cluster to reload.
    let volumes = volume_guard.volumes(&["fakecloud-elasticache=data-cache"]);
    assert_eq!(volumes.len(), 1, "one scoped volume: {volumes:?}");
    client
        .delete_cache_cluster()
        .cache_cluster_id("data-cache")
        .send()
        .await
        .unwrap();
    let mut gone = false;
    for _ in 0..60 {
        if !helpers::volume_exists(&volumes[0]) {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        gone,
        "DeleteCacheCluster left data volume {} behind",
        volumes[0]
    );
}

/// Issue #2630: a fresh `--data-path` creating a cluster with an id another
/// data dir used must not reload that data dir's RDB.
#[tokio::test]
async fn fresh_data_dir_does_not_reload_another_dirs_cache() {
    if !require_docker_or_skip("fresh_data_dir_does_not_reload_another_dirs_cache") {
        return;
    }
    let id = "isolated-cache";
    let create = |client: aws_sdk_elasticache::Client| async move {
        client
            .create_cache_cluster()
            .cache_cluster_id(id)
            .engine("redis")
            .cache_node_type("cache.t3.micro")
            .preferred_availability_zone("us-east-1a")
            .send()
            .await
            .unwrap();
        helpers::wait_for_cache_cluster_available(&client, id, 120).await;
        wait_cache_port(&client, id).await
    };

    let tmp_a = tempfile::tempdir().unwrap();
    let path_a = tmp_a.path().display().to_string();
    let _volumes_a = helpers::DataVolumeGuard::new(tmp_a.path());
    {
        let args = ["--storage-mode", "persistent", "--data-path", &path_a];
        let server = TestServer::start_full(&[], &args).await;
        let port = create(server.elasticache_client().await).await;
        let mut s = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .expect("connect redis to seed");
        let set = redis_cmd(
            &mut s,
            b"*3\r\n$3\r\nSET\r\n$10\r\nonly-in-a!\r\n$1\r\n1\r\n",
        )
        .await;
        assert!(set.starts_with("+OK"), "SET should succeed: {set:?}");
        let save = redis_cmd(&mut s, b"*1\r\n$4\r\nSAVE\r\n").await;
        assert!(save.starts_with("+OK"), "SAVE should succeed: {save:?}");
    }

    let tmp_b = tempfile::tempdir().unwrap();
    let path_b = tmp_b.path().display().to_string();
    let _volumes_b = helpers::DataVolumeGuard::new(tmp_b.path());
    let args = ["--storage-mode", "persistent", "--data-path", &path_b];
    let server = TestServer::start_full(&[], &args).await;
    let port = create(server.elasticache_client().await).await;
    let mut value = String::new();
    for _ in 0..40 {
        if let Ok(mut s) = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await {
            value = redis_cmd(&mut s, b"*2\r\n$6\r\nEXISTS\r\n$10\r\nonly-in-a!\r\n").await;
            if value.starts_with(':') {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(
        value.trim(),
        ":0",
        "a fresh data dir must not reload another data dir's cache"
    );
}

/// Users and user groups survive a restart.
#[tokio::test]
async fn persistence_round_trip_user_and_group() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let client = server.elasticache_client().await;

    client
        .create_user()
        .user_id("persist-user")
        .user_name("persist-user")
        .engine("redis")
        .access_string("on ~* +@all")
        .no_password_required(true)
        .send()
        .await
        .unwrap();

    client
        .create_user_group()
        .user_group_id("persist-group")
        .engine("redis")
        .user_ids("default")
        .user_ids("persist-user")
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.elasticache_client().await;

    // User survives
    let users = client
        .describe_users()
        .user_id("persist-user")
        .send()
        .await
        .unwrap();
    assert_eq!(users.users().len(), 1);
    assert_eq!(users.users()[0].user_id(), Some("persist-user"));

    // User group survives
    let groups = client
        .describe_user_groups()
        .user_group_id("persist-group")
        .send()
        .await
        .unwrap();
    assert_eq!(groups.user_groups().len(), 1);
    let group = &groups.user_groups()[0];
    assert_eq!(group.user_group_id(), Some("persist-group"));
    assert!(group.user_ids().contains(&"persist-user".to_string()));
    assert!(group.user_ids().contains(&"default".to_string()));
}

/// Subnet groups survive a restart.
#[tokio::test]
async fn persistence_round_trip_subnet_group() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let vpc_subnets = server.default_subnet_ids().await;
    let client = server.elasticache_client().await;

    client
        .create_cache_subnet_group()
        .cache_subnet_group_name("persist-sg")
        .cache_subnet_group_description("Persistence test subnet group")
        .subnet_ids(&vpc_subnets[0])
        .subnet_ids(&vpc_subnets[1])
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.elasticache_client().await;

    let groups = client
        .describe_cache_subnet_groups()
        .cache_subnet_group_name("persist-sg")
        .send()
        .await
        .unwrap();
    let sgs = groups.cache_subnet_groups();
    assert_eq!(sgs.len(), 1);
    assert_eq!(sgs[0].cache_subnet_group_name(), Some("persist-sg"));
    assert_eq!(
        sgs[0].cache_subnet_group_description(),
        Some("Persistence test subnet group")
    );
}

/// Tags survive a restart.
#[tokio::test]
async fn persistence_round_trip_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let client = server.elasticache_client().await;

    client
        .create_user()
        .user_id("tagged-user")
        .user_name("tagged-user")
        .engine("redis")
        .access_string("on ~* +@all")
        .no_password_required(true)
        .send()
        .await
        .unwrap();

    // Get the ARN
    let users = client
        .describe_users()
        .user_id("tagged-user")
        .send()
        .await
        .unwrap();
    let arn = users.users()[0].arn().unwrap().to_string();

    client
        .add_tags_to_resource()
        .resource_name(&arn)
        .tags(
            aws_sdk_elasticache::types::Tag::builder()
                .key("env")
                .value("prod")
                .build(),
        )
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.elasticache_client().await;

    let tags = client
        .list_tags_for_resource()
        .resource_name(&arn)
        .send()
        .await
        .unwrap();
    assert!(tags
        .tag_list()
        .iter()
        .any(|t| t.key() == Some("env") && t.value() == Some("prod")));
}

/// Deletion survives a restart.
#[tokio::test]
async fn persistence_deletion_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let client = server.elasticache_client().await;

    client
        .create_user()
        .user_id("doomed-user")
        .user_name("doomed-user")
        .engine("redis")
        .access_string("on ~* +@all")
        .no_password_required(true)
        .send()
        .await
        .unwrap();

    client
        .delete_user()
        .user_id("doomed-user")
        .send()
        .await
        .unwrap();

    drop(client);
    server.restart().await;
    let client = server.elasticache_client().await;

    let users = client.describe_users().send().await.unwrap();
    assert!(
        !users
            .users()
            .iter()
            .any(|u| u.user_id() == Some("doomed-user")),
        "deleted user should not reappear"
    );
}

/// Cache ids are unique per account only: two accounts' caches with the same
/// id get separate containers and data volumes, and a per-account reset tears
/// down only its own account's cache.
#[tokio::test]
async fn same_cache_id_in_two_accounts_is_isolated_and_reset_per_account() {
    if !require_docker_or_skip("same_cache_id_in_two_accounts_is_isolated_and_reset_per_account") {
        return;
    }
    const ACCOUNT_B: &str = "222222222222";
    let id = "shared-id";
    let tmp = tempfile::tempdir().unwrap();
    let data_path = tmp.path().display().to_string();
    let args = ["--storage-mode", "persistent", "--data-path", &data_path];
    let volumes = helpers::DataVolumeGuard::new(tmp.path());
    let server = TestServer::start_full(&[], &args).await;
    let client_a = server.elasticache_client().await;
    let (akid, secret) = server.create_admin(ACCOUNT_B, "cache-admin").await;
    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_elasticache::config::Credentials::new(
            akid,
            secret,
            None,
            None,
            "ec-multi-acct",
        ))
        .load()
        .await;
    let client_b = aws_sdk_elasticache::Client::new(&cfg);

    for client in [&client_a, &client_b] {
        client
            .create_cache_cluster()
            .cache_cluster_id(id)
            .engine("redis")
            .cache_node_type("cache.t3.micro")
            .preferred_availability_zone("us-east-1a")
            .send()
            .await
            .unwrap();
    }
    let port_a = wait_cache_port(&client_a, id).await;
    let port_b = wait_cache_port(&client_b, id).await;
    assert_ne!(port_a, port_b, "each account gets its own container");

    let mut a = tokio::net::TcpStream::connect(format!("127.0.0.1:{port_a}"))
        .await
        .expect("connect account A's redis");
    let set = redis_cmd(&mut a, b"*3\r\n$3\r\nSET\r\n$5\r\nonlyA\r\n$1\r\n1\r\n").await;
    assert!(set.starts_with("+OK"), "SET should succeed: {set:?}");
    let mut b = tokio::net::TcpStream::connect(format!("127.0.0.1:{port_b}"))
        .await
        .expect("connect account B's redis");
    let exists = redis_cmd(&mut b, b"*2\r\n$6\r\nEXISTS\r\n$5\r\nonlyA\r\n").await;
    assert_eq!(
        exists.trim(),
        ":0",
        "account B must not see account A's data"
    );
    drop(b);
    assert_eq!(
        volumes
            .volumes(&[&format!("fakecloud-elasticache={id}")])
            .len(),
        2,
        "one data volume per account"
    );
    let vol_b = volumes.volumes(&[
        &format!("fakecloud-elasticache={id}"),
        &format!("fakecloud-account={ACCOUNT_B}"),
    ]);
    assert_eq!(vol_b.len(), 1);

    // Resetting account B's ElastiCache leaves account A's cache running.
    let reset = reqwest::Client::new()
        .post(format!(
            "{}/_fakecloud/reset/elasticache/{ACCOUNT_B}",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);
    assert!(
        !helpers::volume_exists(&vol_b[0]),
        "account B's volume is removed by the time reset returns"
    );
    let get = redis_cmd(&mut a, b"*2\r\n$3\r\nGET\r\n$5\r\nonlyA\r\n").await;
    assert!(
        get.contains("\r\n1\r\n"),
        "account A's cache must survive account B's reset, got {get:?}"
    );
    assert_eq!(
        volumes
            .volumes(&[&format!("fakecloud-elasticache={id}")])
            .len(),
        1
    );
}
