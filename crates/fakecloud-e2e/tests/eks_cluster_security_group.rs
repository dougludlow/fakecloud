//! EKS creates a real security group for every cluster (returned as
//! `resourcesVpcConfig.clusterSecurityGroupId`) in the cluster's VPC. Tooling
//! such as the `terraform-aws-modules/eks` module tags that group and adds
//! rules to it through EC2, so it must exist there, not only as an id.

use aws_sdk_ec2::types::{Filter, Tag};
use fakecloud_testkit::TestServer;

async fn vpc_and_subnet(ec2: &aws_sdk_ec2::Client) -> (String, String) {
    let vpc = ec2
        .create_vpc()
        .cidr_block("10.0.0.0/16")
        .send()
        .await
        .unwrap()
        .vpc
        .unwrap()
        .vpc_id
        .unwrap();
    let subnet = ec2
        .create_subnet()
        .vpc_id(&vpc)
        .cidr_block("10.0.1.0/24")
        .send()
        .await
        .unwrap()
        .subnet
        .unwrap()
        .subnet_id
        .unwrap();
    (vpc, subnet)
}

fn tag_value<'a>(tags: &'a [Tag], key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|t| t.key() == Some(key))
        .and_then(|t| t.value())
}

#[tokio::test]
async fn eks_cluster_security_group_exists_in_ec2() {
    let server = TestServer::start().await;
    let eks = server.eks_client().await;
    let ec2 = server.ec2_client().await;
    let (vpc, subnet) = vpc_and_subnet(&ec2).await;

    eks.create_cluster()
        .name("demo")
        .role_arn("arn:aws:iam::000000000000:role/eks")
        .resources_vpc_config(
            aws_sdk_eks::types::VpcConfigRequest::builder()
                .subnet_ids(&subnet)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let cluster = eks
        .describe_cluster()
        .name("demo")
        .send()
        .await
        .unwrap()
        .cluster
        .unwrap();
    let cfg = cluster.resources_vpc_config().unwrap();
    assert_eq!(cfg.vpc_id(), Some(vpc.as_str()));
    let sg_id = cfg.cluster_security_group_id().unwrap().to_string();

    let groups = ec2
        .describe_security_groups()
        .group_ids(&sg_id)
        .send()
        .await
        .unwrap();
    let sg = &groups.security_groups()[0];
    assert_eq!(sg.vpc_id(), Some(vpc.as_str()));
    assert!(sg.group_name().unwrap().starts_with("eks-cluster-sg-demo-"));
    assert_eq!(
        sg.description(),
        Some(
            "EKS created security group applied to ENI that is attached to EKS Control Plane \
             master nodes, as well as any managed workloads."
        )
    );
    assert_eq!(tag_value(sg.tags(), "aws:eks:cluster-name"), Some("demo"));
    assert_eq!(
        tag_value(sg.tags(), "kubernetes.io/cluster/demo"),
        Some("owned")
    );
    // Self-referencing all-traffic ingress and all-traffic egress.
    let ingress = sg.ip_permissions();
    assert_eq!(ingress.len(), 1);
    assert_eq!(ingress[0].ip_protocol(), Some("-1"));
    assert_eq!(
        ingress[0].user_id_group_pairs()[0].group_id(),
        Some(sg_id.as_str())
    );
    let egress = sg.ip_permissions_egress();
    assert_eq!(egress.len(), 1);
    assert_eq!(egress[0].ip_ranges()[0].cidr_ip(), Some("0.0.0.0/0"));

    // The group is a normal EC2 resource: it can be tagged (Karpenter
    // discovery) and filtered by the EKS tag.
    ec2.create_tags()
        .resources(&sg_id)
        .tags(
            Tag::builder()
                .key("karpenter.sh/discovery")
                .value("demo")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let by_tag = ec2
        .describe_security_groups()
        .filters(
            Filter::builder()
                .name("tag:aws:eks:cluster-name")
                .values("demo")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let found = &by_tag.security_groups()[0];
    assert_eq!(found.group_id(), Some(sg_id.as_str()));
    assert_eq!(
        tag_value(found.tags(), "karpenter.sh/discovery"),
        Some("demo")
    );

    // Deleting the cluster deletes the group.
    eks.delete_cluster().name("demo").send().await.unwrap();
    let err = ec2
        .describe_security_groups()
        .group_ids(&sg_id)
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidGroup.NotFound")
    );
}

#[tokio::test]
async fn eks_cloudformation_cluster_security_group_exists_in_ec2() {
    let server = TestServer::start().await;
    let ec2 = server.ec2_client().await;
    let cfn = server.cloudformation_client().await;
    let (vpc, subnet) = vpc_and_subnet(&ec2).await;

    let template = format!(
        r#"{{
  "Resources": {{
    "Cluster": {{
      "Type": "AWS::EKS::Cluster",
      "Properties": {{
        "Name": "cfn-demo",
        "RoleArn": "arn:aws:iam::000000000000:role/eks",
        "ResourcesVpcConfig": {{ "SubnetIds": ["{subnet}"] }}
      }}
    }}
  }},
  "Outputs": {{
    "Sg": {{ "Value": {{ "Fn::GetAtt": ["Cluster", "ClusterSecurityGroupId"] }} }}
  }}
}}"#
    );
    cfn.create_stack()
        .stack_name("eks-sg")
        .template_body(template)
        .send()
        .await
        .unwrap();
    let stack = cfn
        .describe_stacks()
        .stack_name("eks-sg")
        .send()
        .await
        .unwrap()
        .stacks()[0]
        .clone();
    let sg_id = stack.outputs()[0].output_value().unwrap().to_string();

    let groups = ec2
        .describe_security_groups()
        .group_ids(&sg_id)
        .send()
        .await
        .unwrap();
    let sg = &groups.security_groups()[0];
    assert_eq!(sg.vpc_id(), Some(vpc.as_str()));
    assert_eq!(
        tag_value(sg.tags(), "aws:eks:cluster-name"),
        Some("cfn-demo")
    );

    cfn.delete_stack()
        .stack_name("eks-sg")
        .send()
        .await
        .unwrap();
    let err = ec2
        .describe_security_groups()
        .group_ids(&sg_id)
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidGroup.NotFound")
    );
}

#[tokio::test]
async fn eks_cluster_security_group_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let ec2 = server.ec2_client().await;
    let (vpc, subnet) = vpc_and_subnet(&ec2).await;

    let created = server
        .eks_client()
        .await
        .create_cluster()
        .name("persisted")
        .role_arn("arn:aws:iam::000000000000:role/eks")
        .resources_vpc_config(
            aws_sdk_eks::types::VpcConfigRequest::builder()
                .subnet_ids(&subnet)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let sg_id = created
        .cluster()
        .and_then(|c| c.resources_vpc_config())
        .and_then(|c| c.cluster_security_group_id())
        .unwrap()
        .to_string();

    server.restart().await;

    let ec2 = server.ec2_client().await;
    let groups = ec2
        .describe_security_groups()
        .group_ids(&sg_id)
        .send()
        .await
        .unwrap();
    let sg = &groups.security_groups()[0];
    assert_eq!(sg.vpc_id(), Some(vpc.as_str()));
    assert_eq!(
        tag_value(sg.tags(), "aws:eks:cluster-name"),
        Some("persisted")
    );
    let cluster = server
        .eks_client()
        .await
        .describe_cluster()
        .name("persisted")
        .send()
        .await
        .unwrap()
        .cluster
        .unwrap();
    assert_eq!(
        cluster
            .resources_vpc_config()
            .and_then(|c| c.cluster_security_group_id()),
        Some(sg_id.as_str())
    );
}
