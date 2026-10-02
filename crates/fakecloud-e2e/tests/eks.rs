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
    assert_eq!(default, "v1.3.4-eksbuild.1");

    // The other AWS-owned add-ons the module commonly installs resolve too.
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
        assert!(!out.addons()[0].addon_versions().is_empty(), "{name}");
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
        Some("v1.3.4-eksbuild.1")
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
    assert_eq!(addon.addon_version(), Some("v1.3.4-eksbuild.1"));
}
