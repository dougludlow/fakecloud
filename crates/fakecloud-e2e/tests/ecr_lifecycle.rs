//! ECR lifecycle-policy evaluator. Covers the rule shapes that the
//! unit suite exercises end-to-end against the real AWS SDK to catch
//! serialization / dispatch regressions on top of evaluator bugs.

mod helpers;

use helpers::TestServer;

async fn create_repo(ecr: &aws_sdk_ecr::Client, name: &str) {
    ecr.create_repository()
        .repository_name(name)
        .send()
        .await
        .expect("create_repository");
}

/// Push N empty images with the given tags. Uses raw OCI v2 via HTTP
/// — the AWS SDK `PutImage` path works too but is noisier for this
/// test's shape.
async fn push_tagged_manifests(endpoint: &str, repo: &str, tags: &[&str]) {
    let http = reqwest::Client::new();
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("AWS:test")
    );
    for (i, tag) in tags.iter().enumerate() {
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {
                "mediaType": "application/vnd.docker.container.image.v1+json",
                "size": 0,
                "digest": format!(
                    "sha256:{:0>64}",
                    format!("lifecycle-{repo}-{i}-{tag}").replace('-', "")
                )
            },
            "layers": []
        });
        let body = serde_json::to_vec(&manifest).unwrap();
        let resp = http
            .put(format!("{endpoint}/v2/{repo}/manifests/{tag}"))
            .header("Authorization", &auth)
            .header(
                "Content-Type",
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED, "tag={tag}");
    }
}

use base64::Engine;

/// tagPrefixList limits countType evaluation to matching tags.
#[tokio::test]
async fn lifecycle_tag_prefix_list_prunes_matching_only() {
    let server = TestServer::start().await;
    let ecr = server.ecr_client().await;
    create_repo(&ecr, "lifecycle-prefix").await;
    push_tagged_manifests(
        server.endpoint(),
        "lifecycle-prefix",
        &["dev-1", "dev-2", "dev-3", "prod-1"],
    )
    .await;

    // Keep newest 1 of dev-*; prod-* untouched.
    let policy = serde_json::json!({
        "rules": [{
            "rulePriority": 1,
            "selection": {
                "tagStatus": "tagged",
                "tagPrefixList": ["dev-"],
                "countType": "imageCountMoreThan",
                "countNumber": 1,
            },
            "action": {"type": "expire"}
        }]
    });
    ecr.put_lifecycle_policy()
        .repository_name("lifecycle-prefix")
        .lifecycle_policy_text(policy.to_string())
        .send()
        .await
        .expect("put_lifecycle_policy");

    let described = ecr
        .describe_images()
        .repository_name("lifecycle-prefix")
        .send()
        .await
        .expect("describe_images");
    let remaining_tags: Vec<String> = described
        .image_details()
        .iter()
        .flat_map(|d| d.image_tags().iter().map(|t| t.to_string()))
        .collect();
    // dev-3 (newest dev) survives + all prod-*; dev-1, dev-2 pruned.
    assert!(
        remaining_tags.iter().any(|t| t == "dev-3"),
        "dev-3 missing from {remaining_tags:?}"
    );
    assert!(
        remaining_tags.iter().any(|t| t == "prod-1"),
        "prod-1 missing from {remaining_tags:?}"
    );
    assert!(
        !remaining_tags.iter().any(|t| t == "dev-1"),
        "dev-1 should have been pruned"
    );
    assert!(
        !remaining_tags.iter().any(|t| t == "dev-2"),
        "dev-2 should have been pruned"
    );
}

/// tagPatternList uses `*` wildcards.
#[tokio::test]
async fn lifecycle_tag_pattern_list_prunes_matching() {
    let server = TestServer::start().await;
    let ecr = server.ecr_client().await;
    create_repo(&ecr, "lifecycle-pattern").await;
    push_tagged_manifests(
        server.endpoint(),
        "lifecycle-pattern",
        &["release-2024", "release-2025", "hotfix-2025"],
    )
    .await;

    let policy = serde_json::json!({
        "rules": [{
            "rulePriority": 1,
            "selection": {
                "tagStatus": "tagged",
                "tagPatternList": ["release-*"],
                "countType": "imageCountMoreThan",
                // AWS rejects countNumber 0; keep one matching image.
                "countNumber": 1,
            },
            "action": {"type": "expire"}
        }]
    });
    ecr.put_lifecycle_policy()
        .repository_name("lifecycle-pattern")
        .lifecycle_policy_text(policy.to_string())
        .send()
        .await
        .expect("put_lifecycle_policy");

    let described = ecr
        .describe_images()
        .repository_name("lifecycle-pattern")
        .send()
        .await
        .expect("describe_images");
    let remaining_tags: Vec<String> = described
        .image_details()
        .iter()
        .flat_map(|d| d.image_tags().iter().map(|t| t.to_string()))
        .collect();
    assert!(
        remaining_tags.iter().any(|t| t == "hotfix-2025"),
        "hotfix-2025 missing from {remaining_tags:?}"
    );
    let releases = remaining_tags
        .iter()
        .filter(|t| t.starts_with("release-"))
        .count();
    assert_eq!(
        releases, 1,
        "exactly one release-* image should survive: {remaining_tags:?}"
    );
}

/// GetLifecyclePolicy round-trips the JSON we stored.
#[tokio::test]
async fn lifecycle_policy_roundtrips() {
    let server = TestServer::start().await;
    let ecr = server.ecr_client().await;
    create_repo(&ecr, "lifecycle-roundtrip").await;
    let policy = r#"{"rules":[{"rulePriority":1,"selection":{"tagStatus":"any","countType":"sinceImagePushed","countUnit":"days","countNumber":30},"action":{"type":"expire"}}]}"#;
    ecr.put_lifecycle_policy()
        .repository_name("lifecycle-roundtrip")
        .lifecycle_policy_text(policy)
        .send()
        .await
        .unwrap();
    let got = ecr
        .get_lifecycle_policy()
        .repository_name("lifecycle-roundtrip")
        .send()
        .await
        .unwrap();
    let stored: serde_json::Value =
        serde_json::from_str(got.lifecycle_policy_text().unwrap()).unwrap();
    assert_eq!(
        stored["rules"][0]["selection"]["countType"],
        serde_json::Value::String("sinceImagePushed".into())
    );
    assert_eq!(
        stored["rules"][0]["selection"]["countUnit"],
        serde_json::Value::String("days".into())
    );
}
