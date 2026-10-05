//! End-to-end tests for EKS, driven through the real `aws-sdk-eks` client
//! against a live fakecloud server.

use fakecloud_testkit::TestServer;

/// The `aws_eks_addon_version` Terraform data source (used by
/// `terraform-aws-modules/eks` for every add-on it installs) calls
/// DescribeAddonVersions with an add-on name and Kubernetes version, then
/// picks the version whose compatibility is flagged `defaultVersion`. An empty
/// result fails the plan with "empty result".
#[tokio::test]
async fn eks_describe_addon_versions_resolves_pod_identity_agent_default() {
    let server = TestServer::start().await;
    let client = server.eks_client().await;

    let out = client
        .describe_addon_versions()
        .addon_name("eks-pod-identity-agent")
        .kubernetes_version("1.32")
        .send()
        .await
        .unwrap();
    let addons = out.addons();
    assert_eq!(addons.len(), 1);
    let addon = &addons[0];
    assert_eq!(addon.addon_name(), Some("eks-pod-identity-agent"));
    assert_eq!(addon.r#type(), Some("security"));
    assert_eq!(addon.owner(), Some("aws"));
    assert_eq!(addon.publisher(), Some("eks"));

    let default = addon
        .addon_versions()
        .iter()
        .find(|v| {
            v.compatibilities()
                .iter()
                .any(|c| c.cluster_version() == Some("1.32") && c.default_version())
        })
        .and_then(|v| v.addon_version())
        .expect("a default version for 1.32");
    assert_eq!(default, "v1.3.10-eksbuild.1");

    // The other add-ons the module commonly installs resolve to a 1.32
    // default too.
    for name in [
        "metrics-server",
        "snapshot-controller",
        "amazon-cloudwatch-observability",
    ] {
        let out = client
            .describe_addon_versions()
            .addon_name(name)
            .kubernetes_version("1.32")
            .send()
            .await
            .unwrap();
        assert_eq!(out.addons().len(), 1, "{name}");
        let defaults = out.addons()[0]
            .addon_versions()
            .iter()
            .filter(|v| {
                v.compatibilities()
                    .iter()
                    .any(|c| c.cluster_version() == Some("1.32") && c.default_version())
            })
            .count();
        assert_eq!(defaults, 1, "{name} needs exactly one 1.32 default");
    }
}

#[tokio::test]
async fn eks_create_addon_pod_identity_agent_uses_default_version() {
    let server = TestServer::start().await;
    let client = server.eks_client().await;

    client
        .create_cluster()
        .name("addons")
        .role_arn("arn:aws:iam::000000000000:role/eks")
        .version("1.32")
        .resources_vpc_config(
            aws_sdk_eks::types::VpcConfigRequest::builder()
                .subnet_ids("subnet-12345678")
                .build(),
        )
        .send()
        .await
        .unwrap();

    let created = client
        .create_addon()
        .cluster_name("addons")
        .addon_name("eks-pod-identity-agent")
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.addon().and_then(|a| a.addon_version()),
        Some("v1.3.10-eksbuild.1")
    );

    let described = client
        .describe_addon()
        .cluster_name("addons")
        .addon_name("eks-pod-identity-agent")
        .send()
        .await
        .unwrap();
    let addon = described.addon().unwrap();
    assert_eq!(addon.addon_name(), Some("eks-pod-identity-agent"));
    assert_eq!(addon.addon_version(), Some("v1.3.10-eksbuild.1"));
}

/// DescribeClusterVersions reports the AWS support windows and the deprecated
/// `status` member in its own `ClusterVersionStatus` enum (`standard-support`),
/// which the SDK only decodes to a known variant when the wire value matches.
// The deprecated `status` member is exactly what this test pins down.
#[allow(deprecated)]
#[tokio::test]
async fn eks_describe_cluster_versions_matches_aws_lifecycle() {
    use aws_sdk_eks::types::{ClusterVersionStatus, VersionStatus};

    let server = TestServer::start().await;
    let client = server.eks_client().await;

    let out = client.describe_cluster_versions().send().await.unwrap();
    let versions = out.cluster_versions();
    let find = |v: &str| {
        versions
            .iter()
            .find(|c| c.cluster_version() == Some(v))
            .unwrap_or_else(|| panic!("missing {v}"))
    };

    let v134 = find("1.34");
    assert_eq!(v134.status(), Some(&ClusterVersionStatus::StandardSupport));
    assert_eq!(v134.version_status(), Some(&VersionStatus::StandardSupport));
    assert_eq!(v134.release_date().unwrap().secs(), 1_759_363_200); // 2025-10-02
    assert_eq!(
        v134.end_of_standard_support_date().unwrap().secs(),
        1_796_169_600 // 2026-12-02
    );
    assert_eq!(
        v134.end_of_extended_support_date().unwrap().secs(),
        1_827_705_600 // 2027-12-02
    );
    assert_eq!(v134.kubernetes_patch_version(), Some("1.34.1"));

    let v133 = find("1.33");
    assert_eq!(v133.release_date().unwrap().secs(), 1_748_476_800); // 2025-05-29
    assert_eq!(
        v133.end_of_standard_support_date().unwrap().secs(),
        1_785_283_200 // 2026-07-29
    );

    let v130 = find("1.30");
    assert_eq!(v130.status(), Some(&ClusterVersionStatus::ExtendedSupport));
    assert_eq!(v130.version_status(), Some(&VersionStatus::ExtendedSupport));

    // The deprecated `status` filter takes the same enum.
    let extended = client
        .describe_cluster_versions()
        .status(ClusterVersionStatus::ExtendedSupport)
        .send()
        .await
        .unwrap();
    let names: Vec<&str> = extended
        .cluster_versions()
        .iter()
        .filter_map(|c| c.cluster_version())
        .collect();
    assert_eq!(names, vec!["1.28", "1.29", "1.30"]);
}

/// The add-on catalog carries AWS's real version strings and the add-ons EKS
/// offered on the pinned date, and DescribeAddonConfiguration recommends the
/// documented pod identity setup.
#[tokio::test]
async fn eks_addon_catalog_matches_aws_snapshot() {
    let server = TestServer::start().await;
    let client = server.eks_client().await;

    let versions_of =
        |out: &aws_sdk_eks::operation::describe_addon_versions::DescribeAddonVersionsOutput| {
            out.addons()[0]
                .addon_versions()
                .iter()
                .filter_map(|v| v.addon_version().map(str::to_string))
                .collect::<Vec<_>>()
        };

    let kp = client
        .describe_addon_versions()
        .addon_name("kube-proxy")
        .kubernetes_version("1.33")
        .send()
        .await
        .unwrap();
    let kp_versions = versions_of(&kp);
    assert_eq!(kp_versions[0], "v1.33.5-eksbuild.2");
    assert!(!kp_versions.iter().any(|v| v == "v1.33.0-eksbuild.3"));

    let coredns = client
        .describe_addon_versions()
        .addon_name("coredns")
        .kubernetes_version("1.34")
        .send()
        .await
        .unwrap();
    assert_eq!(versions_of(&coredns)[0], "v1.12.4-eksbuild.1");

    let mountpoint = client
        .describe_addon_versions()
        .addon_name("aws-mountpoint-s3-csi-driver")
        .send()
        .await
        .unwrap();
    assert_eq!(mountpoint.addons()[0].publisher(), Some("s3"));

    for (name, namespace) in [
        (
            "aws-privateca-connector-for-kubernetes",
            "aws-privateca-issuer",
        ),
        ("fluent-bit", "fluent-bit"),
        ("sriov-network-metrics-exporter", "monitoring"),
        (
            "amazon-sagemaker-hyperpod-training-operator",
            "aws-hyperpod",
        ),
    ] {
        let out = client
            .describe_addon_versions()
            .addon_name(name)
            .kubernetes_version("1.31")
            .send()
            .await
            .unwrap();
        assert_eq!(out.addons().len(), 1, "{name}");
        assert_eq!(
            out.addons()[0].default_namespace(),
            Some(namespace),
            "{name}"
        );
    }

    let cfg = client
        .describe_addon_configuration()
        .addon_name("external-dns")
        .addon_version("v0.20.0-eksbuild.1")
        .send()
        .await
        .unwrap();
    let pod_identity = cfg.pod_identity_configuration();
    assert_eq!(pod_identity.len(), 1);
    assert_eq!(pod_identity[0].service_account(), Some("external-dns"));
    assert_eq!(
        pod_identity[0].recommended_managed_policies(),
        ["arn:aws:iam::aws:policy/AmazonRoute53FullAccess".to_string()]
    );
}

/// EKS refuses an add-on with no build for the cluster's Kubernetes version
/// and a requested version not offered for it, rather than installing a build
/// for another minor.
#[tokio::test]
async fn eks_addon_versions_are_checked_against_cluster_version() {
    use aws_sdk_eks::error::ProvideErrorMetadata;

    let server = TestServer::start().await;
    let client = server.eks_client().await;

    client
        .create_cluster()
        .name("k134")
        .role_arn("arn:aws:iam::000000000000:role/eks")
        .version("1.34")
        .resources_vpc_config(
            aws_sdk_eks::types::VpcConfigRequest::builder()
                .subnet_ids("subnet-12345678")
                .build(),
        )
        .send()
        .await
        .unwrap();

    // adot has no 1.34 build.
    let err = client
        .create_addon()
        .cluster_name("k134")
        .addon_name("adot")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(err.is_invalid_parameter_exception(), "{err:?}");
    assert_eq!(err.message(), Some("Addon specified is not supported"));

    // The 1.34 default, not the default cluster version's.
    let created = client
        .create_addon()
        .cluster_name("k134")
        .addon_name("kube-proxy")
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.addon().and_then(|a| a.addon_version()),
        Some("v1.34.0-eksbuild.2")
    );

    // A build that never existed is refused on update; the add-on keeps its
    // version.
    let err = client
        .update_addon()
        .cluster_name("k134")
        .addon_name("kube-proxy")
        .addon_version("v1.34.0-eksbuild.3")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert!(err.is_invalid_parameter_exception(), "{err:?}");
    assert_eq!(
        err.message(),
        Some("Addon version specified is not supported")
    );
    let described = client
        .describe_addon()
        .cluster_name("k134")
        .addon_name("kube-proxy")
        .send()
        .await
        .unwrap();
    assert_eq!(
        described.addon().and_then(|a| a.addon_version()),
        Some("v1.34.0-eksbuild.2")
    );

    let updated = client
        .update_addon()
        .cluster_name("k134")
        .addon_name("kube-proxy")
        .addon_version("v1.34.1-eksbuild.2")
        .send()
        .await
        .unwrap();
    assert!(updated.update().is_some());
}
