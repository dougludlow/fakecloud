//! Resources that live in a customer VPC report ids EC2 knows about.
//!
//! Load balancers, DB / cache / cluster subnet groups, EFS mount targets,
//! brokers, EKS node groups and add-ons, Lambda VPC configs and OpenSearch
//! domains all take subnets (and security groups) created through EC2. Their
//! `VpcId`, Availability Zones, security groups and network interfaces must
//! resolve back through EC2 `Describe*` (and Auto Scaling / EKS for the node
//! group and pod identity records), the way Terraform round-trips them.

mod helpers;

use aws_sdk_ec2::types::Filter;
use aws_sdk_elasticloadbalancingv2::types::{
    Action, ActionTypeEnum, AuthenticateOidcActionConfig, Certificate, ForwardActionConfig,
    LoadBalancerTypeEnum, ProtocolEnum, SubnetMapping, TargetGroupStickinessConfig,
    TargetGroupTuple,
};
use helpers::TestServer;

/// A server with no container runtime: nothing here needs a data plane.
async fn start() -> TestServer {
    TestServer::start_with_env(&[("FAKECLOUD_CONTAINER_CLI", "false")]).await
}

/// A fresh VPC with two subnets in different Availability Zones. Returns
/// `(vpc_id, [(subnet_id, az, cidr)])`.
async fn vpc_with_subnets(
    server: &TestServer,
    cidr: &str,
) -> (String, Vec<(String, String, String)>) {
    let ec2 = server.ec2_client().await;
    let vpc_id = ec2
        .create_vpc()
        .cidr_block(format!("{cidr}.0.0/16"))
        .send()
        .await
        .unwrap()
        .vpc()
        .unwrap()
        .vpc_id()
        .unwrap()
        .to_string();
    let mut subnets = Vec::new();
    for (i, az) in ["us-east-1a", "us-east-1b"].iter().enumerate() {
        let block = format!("{cidr}.{}.0/24", i + 1);
        let subnet = ec2
            .create_subnet()
            .vpc_id(&vpc_id)
            .cidr_block(&block)
            .availability_zone(*az)
            .send()
            .await
            .unwrap();
        subnets.push((
            subnet.subnet().unwrap().subnet_id().unwrap().to_string(),
            az.to_string(),
            block,
        ));
    }
    (vpc_id, subnets)
}

async fn default_sg_of(server: &TestServer, vpc_id: &str) -> String {
    let ec2 = server.ec2_client().await;
    let groups = ec2
        .describe_security_groups()
        .filters(Filter::builder().name("vpc-id").values(vpc_id).build())
        .filters(
            Filter::builder()
                .name("group-name")
                .values("default")
                .build(),
        )
        .send()
        .await
        .unwrap();
    groups.security_groups()[0].group_id().unwrap().to_string()
}

#[tokio::test]
async fn load_balancer_vpc_azs_security_groups_and_zone_come_from_ec2() {
    let server = start().await;
    let (vpc_id, subnets) = vpc_with_subnets(&server, "10.40").await;
    let default_sg = default_sg_of(&server, &vpc_id).await;
    let elb = server.elbv2_client().await;

    let alb = elb
        .create_load_balancer()
        .name("placed-alb")
        .subnets(&subnets[0].0)
        .subnets(&subnets[1].0)
        .send()
        .await
        .unwrap();
    let lb = &alb.load_balancers()[0];
    assert_eq!(lb.vpc_id(), Some(vpc_id.as_str()));
    assert_eq!(lb.canonical_hosted_zone_id(), Some("Z35SXDOTRQ7X7K"));
    for (subnet, az, _) in &subnets {
        assert!(lb
            .availability_zones()
            .iter()
            .any(|z| z.subnet_id() == Some(subnet.as_str()) && z.zone_name() == Some(az.as_str())));
    }
    // An ALB created without security groups gets the VPC's default group.
    assert_eq!(lb.security_groups(), [default_sg]);

    // An NLB keeps the pinned private IP and uses the NLB hosted zone.
    let nlb = elb
        .create_load_balancer()
        .name("placed-nlb")
        .r#type(LoadBalancerTypeEnum::Network)
        .subnet_mappings(
            SubnetMapping::builder()
                .subnet_id(&subnets[0].0)
                .private_ipv4_address("10.40.1.10")
                .build(),
        )
        .send()
        .await
        .unwrap();
    let lb = &nlb.load_balancers()[0];
    assert_eq!(lb.canonical_hosted_zone_id(), Some("Z26RNL4JYFTOTI"));
    assert_eq!(
        lb.availability_zones()[0].load_balancer_addresses()[0].private_ipv4_address(),
        Some("10.40.1.10")
    );

    // Subnets and security groups that EC2 does not know are rejected.
    let err = elb
        .create_load_balancer()
        .name("ghost-subnet")
        .subnets("subnet-0000000000000dead")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("SubnetNotFound")
    );
    let err = elb
        .create_load_balancer()
        .name("ghost-sg")
        .subnets(&subnets[0].0)
        .security_groups("sg-0000000000000dead")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidSecurityGroup")
    );
}

#[tokio::test]
async fn listener_actions_round_trip_oidc_and_forward_stickiness() {
    let server = start().await;
    let (vpc_id, subnets) = vpc_with_subnets(&server, "10.41").await;
    let elb = server.elbv2_client().await;
    let lb_arn = elb
        .create_load_balancer()
        .name("auth-alb")
        .subnets(&subnets[0].0)
        .subnets(&subnets[1].0)
        .send()
        .await
        .unwrap()
        .load_balancers()[0]
        .load_balancer_arn()
        .unwrap()
        .to_string();
    let tg_arn = elb
        .create_target_group()
        .name("auth-tg")
        .protocol(ProtocolEnum::Http)
        .port(80)
        .vpc_id(&vpc_id)
        .send()
        .await
        .unwrap()
        .target_groups()[0]
        .target_group_arn()
        .unwrap()
        .to_string();
    let oidc = AuthenticateOidcActionConfig::builder()
        .issuer("https://idp.example.com")
        .authorization_endpoint("https://idp.example.com/authorize")
        .token_endpoint("https://idp.example.com/token")
        .user_info_endpoint("https://idp.example.com/userinfo")
        .client_id("client-1")
        .client_secret("very-secret")
        .session_timeout(3600)
        .build();
    let listener_arn = elb
        .create_listener()
        .load_balancer_arn(&lb_arn)
        .protocol(ProtocolEnum::Https)
        .port(443)
        .certificates(
            Certificate::builder()
                .certificate_arn("arn:aws:acm:us-east-1:000000000000:certificate/abc")
                .build(),
        )
        .default_actions(
            Action::builder()
                .r#type(ActionTypeEnum::AuthenticateOidc)
                .order(1)
                .authenticate_oidc_config(oidc)
                .build(),
        )
        .default_actions(
            Action::builder()
                .r#type(ActionTypeEnum::Forward)
                .order(2)
                .forward_config(
                    ForwardActionConfig::builder()
                        .target_groups(
                            TargetGroupTuple::builder()
                                .target_group_arn(&tg_arn)
                                .build(),
                        )
                        .target_group_stickiness_config(
                            TargetGroupStickinessConfig::builder()
                                .enabled(true)
                                .duration_seconds(900)
                                .build(),
                        )
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap()
        .listeners()[0]
        .listener_arn()
        .unwrap()
        .to_string();

    let described = elb
        .describe_listeners()
        .listener_arns(&listener_arn)
        .send()
        .await
        .unwrap();
    let actions = described.listeners()[0].default_actions();
    let auth = actions
        .iter()
        .find(|a| a.r#type() == Some(&ActionTypeEnum::AuthenticateOidc))
        .and_then(|a| a.authenticate_oidc_config())
        .expect("AuthenticateOidcConfig round-trips");
    assert_eq!(auth.issuer(), Some("https://idp.example.com"));
    assert_eq!(auth.client_id(), Some("client-1"));
    assert_eq!(auth.session_timeout(), Some(3600));
    // AWS never returns the client secret.
    assert_eq!(auth.client_secret(), None);
    let stickiness = actions
        .iter()
        .find(|a| a.r#type() == Some(&ActionTypeEnum::Forward))
        .and_then(|a| a.forward_config())
        .and_then(|f| f.target_group_stickiness_config())
        .expect("TargetGroupStickinessConfig round-trips");
    assert_eq!(stickiness.enabled(), Some(true));
    assert_eq!(stickiness.duration_seconds(), Some(900));
}

#[tokio::test]
async fn database_subnet_groups_report_the_subnets_vpc_and_zones() {
    let server = start().await;
    let (vpc_id, subnets) = vpc_with_subnets(&server, "10.42").await;

    let rds = server.rds_client().await;
    let group = rds
        .create_db_subnet_group()
        .db_subnet_group_name("placed-rds")
        .db_subnet_group_description("d")
        .subnet_ids(&subnets[0].0)
        .subnet_ids(&subnets[1].0)
        .send()
        .await
        .unwrap();
    let group = group.db_subnet_group().unwrap();
    assert_eq!(group.vpc_id(), Some(vpc_id.as_str()));
    for (subnet, az, _) in &subnets {
        assert!(group
            .subnets()
            .iter()
            .any(|s| s.subnet_identifier() == Some(subnet.as_str())
                && s.subnet_availability_zone().and_then(|z| z.name()) == Some(az.as_str())));
    }
    let err = rds
        .create_db_subnet_group()
        .db_subnet_group_name("ghost-rds")
        .db_subnet_group_description("d")
        .subnet_ids(&subnets[0].0)
        .subnet_ids("subnet-0000000000000dead")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidSubnet")
    );
    // Subnets from two VPCs cannot share a group.
    let default_subnets = server.default_subnet_ids().await;
    // A group's VPC is fixed: modifying it onto another VPC's subnets fails
    // and leaves it unchanged.
    let err = rds
        .modify_db_subnet_group()
        .db_subnet_group_name("placed-rds")
        .db_subnet_group_description("moved")
        .subnet_ids(&default_subnets[0])
        .subnet_ids(&default_subnets[1])
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidSubnet")
    );
    let unchanged = rds
        .describe_db_subnet_groups()
        .db_subnet_group_name("placed-rds")
        .send()
        .await
        .unwrap();
    let unchanged = &unchanged.db_subnet_groups()[0];
    assert_eq!(unchanged.vpc_id(), Some(vpc_id.as_str()));
    assert_eq!(unchanged.db_subnet_group_description(), Some("d"));
    let err = rds
        .create_db_subnet_group()
        .db_subnet_group_name("split-rds")
        .db_subnet_group_description("d")
        .subnet_ids(&subnets[0].0)
        .subnet_ids(&default_subnets[1])
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidSubnet")
    );

    let elasticache = server.elasticache_client().await;
    let group = elasticache
        .create_cache_subnet_group()
        .cache_subnet_group_name("placed-cache")
        .cache_subnet_group_description("d")
        .subnet_ids(&subnets[0].0)
        .subnet_ids(&subnets[1].0)
        .send()
        .await
        .unwrap();
    let group = group.cache_subnet_group().unwrap();
    assert_eq!(group.vpc_id(), Some(vpc_id.as_str()));
    let err = elasticache
        .modify_cache_subnet_group()
        .cache_subnet_group_name("placed-cache")
        .subnet_ids(&default_subnets[0])
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidSubnet")
    );
    assert!(group
        .subnets()
        .iter()
        .any(|s| s.subnet_identifier() == Some(subnets[1].0.as_str())
            && s.subnet_availability_zone().and_then(|z| z.name()) == Some("us-east-1b")));

    let docdb = server.docdb_client().await;
    let group = docdb
        .create_db_subnet_group()
        .db_subnet_group_name("placed-docdb")
        .db_subnet_group_description("d")
        .subnet_ids(&subnets[0].0)
        .subnet_ids(&subnets[1].0)
        .send()
        .await
        .unwrap();
    assert_eq!(
        group.db_subnet_group().and_then(|g| g.vpc_id()),
        Some(vpc_id.as_str())
    );

    let neptune = server.neptune_client().await;
    let group = neptune
        .create_db_subnet_group()
        .db_subnet_group_name("placed-neptune")
        .db_subnet_group_description("d")
        .subnet_ids(&subnets[0].0)
        .subnet_ids(&subnets[1].0)
        .send()
        .await
        .unwrap();
    assert_eq!(
        group.db_subnet_group().and_then(|g| g.vpc_id()),
        Some(vpc_id.as_str())
    );

    let memorydb = server.memorydb_client().await;
    let group = memorydb
        .create_subnet_group()
        .subnet_group_name("placed-memorydb")
        .subnet_ids(&subnets[0].0)
        .subnet_ids(&subnets[1].0)
        .send()
        .await
        .unwrap();
    let group = group.subnet_group().unwrap();
    assert_eq!(group.vpc_id(), Some(vpc_id.as_str()));
    assert!(group
        .subnets()
        .iter()
        .any(|s| s.identifier() == Some(subnets[0].0.as_str())
            && s.availability_zone().and_then(|z| z.name()) == Some("us-east-1a")));

    // Redshift and DMS through the CLI.
    let out = server
        .aws_cli(&[
            "redshift",
            "create-cluster-subnet-group",
            "--cluster-subnet-group-name",
            "placed-redshift",
            "--description",
            "d",
            "--subnet-ids",
            &subnets[0].0,
            &subnets[1].0,
        ])
        .await;
    assert!(out.success(), "{}", out.stderr_text());
    assert_eq!(
        out.stdout_json()["ClusterSubnetGroup"]["VpcId"],
        vpc_id.as_str()
    );
    let out = server
        .aws_cli(&[
            "dms",
            "create-replication-subnet-group",
            "--replication-subnet-group-identifier",
            "placed-dms",
            "--replication-subnet-group-description",
            "d",
            "--subnet-ids",
            &subnets[0].0,
            &subnets[1].0,
        ])
        .await;
    assert!(out.success(), "{}", out.stderr_text());
    assert_eq!(
        out.stdout_json()["ReplicationSubnetGroup"]["VpcId"],
        vpc_id.as_str()
    );
}

#[tokio::test]
async fn efs_mount_target_creates_and_deletes_a_real_network_interface() {
    let server = start().await;
    let (vpc_id, subnets) = vpc_with_subnets(&server, "10.43").await;
    let default_sg = default_sg_of(&server, &vpc_id).await;
    let efs = aws_sdk_efs::Client::new(&server.aws_config().await);
    let ec2 = server.ec2_client().await;

    let fs_id = efs
        .create_file_system()
        .creation_token("placed-efs")
        .send()
        .await
        .unwrap()
        .file_system_id()
        .to_string();
    // DescribeFileSystems settles `creating` -> `available`.
    efs.describe_file_systems()
        .file_system_id(&fs_id)
        .send()
        .await
        .unwrap();

    let mt = efs
        .create_mount_target()
        .file_system_id(&fs_id)
        .subnet_id(&subnets[0].0)
        .send()
        .await
        .unwrap();
    let eni_id = mt.network_interface_id().unwrap().to_string();
    let ip = mt.ip_address().unwrap().to_string();
    assert_eq!(mt.vpc_id(), Some(vpc_id.as_str()));
    assert!(ip.starts_with("10.43.1."), "{ip} is in {}", subnets[0].2);

    let enis = ec2
        .describe_network_interfaces()
        .network_interface_ids(&eni_id)
        .send()
        .await
        .unwrap();
    let eni = &enis.network_interfaces()[0];
    assert_eq!(eni.subnet_id(), Some(subnets[0].0.as_str()));
    assert_eq!(eni.vpc_id(), Some(vpc_id.as_str()));
    assert_eq!(eni.private_ip_address(), Some(ip.as_str()));
    assert_eq!(eni.requester_managed(), Some(true));
    assert_eq!(
        eni.groups()
            .iter()
            .filter_map(|g| g.group_id())
            .collect::<Vec<_>>(),
        [default_sg.as_str()]
    );
    let mt_id = mt.mount_target_id().to_string();
    assert_eq!(
        eni.description(),
        Some(format!("EFS mount target for {fs_id} ({mt_id})").as_str())
    );

    // The interface belongs to EFS; the account cannot delete it.
    let err = ec2
        .delete_network_interface()
        .network_interface_id(&eni_id)
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidParameterValue")
    );

    // Security groups must exist.
    let err = efs
        .create_mount_target()
        .file_system_id(&fs_id)
        .subnet_id(&subnets[1].0)
        .security_groups("sg-0000000000000dead")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("SecurityGroupNotFound")
    );

    efs.delete_mount_target()
        .mount_target_id(&mt_id)
        .send()
        .await
        .unwrap();
    let err = ec2
        .describe_network_interfaces()
        .network_interface_ids(&eni_id)
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidNetworkInterfaceID.NotFound")
    );
}

#[tokio::test]
async fn broker_without_subnets_lands_in_default_vpc_subnets() {
    use aws_sdk_mq::types::{DeploymentMode, EngineType, User};
    let server = start().await;
    let default_subnets = server.default_subnet_ids().await;
    let default_sg = server.default_security_group_id().await;
    let mq = aws_sdk_mq::Client::new(&server.aws_config().await);
    let user = || {
        User::builder()
            .username("fcadmin")
            .password("FakecloudMQ1234")
            .build()
    };
    let id = mq
        .create_broker()
        .broker_name("placed-broker")
        .engine_type(EngineType::Activemq)
        .host_instance_type("mq.t3.micro")
        .deployment_mode(DeploymentMode::ActiveStandbyMultiAz)
        .publicly_accessible(false)
        .auto_minor_version_upgrade(false)
        .users(user())
        .send()
        .await
        .unwrap()
        .broker_id()
        .unwrap()
        .to_string();
    let broker = mq.describe_broker().broker_id(&id).send().await.unwrap();
    assert_eq!(broker.subnet_ids(), &default_subnets[..2]);
    assert_eq!(broker.security_groups(), [default_sg]);

    let err = mq
        .create_broker()
        .broker_name("ghost-broker")
        .engine_type(EngineType::Activemq)
        .host_instance_type("mq.t3.micro")
        .deployment_mode(DeploymentMode::SingleInstance)
        .publicly_accessible(false)
        .auto_minor_version_upgrade(false)
        .subnet_ids("subnet-0000000000000dead")
        .users(user())
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("BadRequestException")
    );
}

#[tokio::test]
async fn eks_node_group_auto_scaling_group_and_addon_pod_identities_resolve() {
    use aws_sdk_eks::types::{
        AddonPodIdentityAssociations, NodegroupScalingConfig, VpcConfigRequest,
    };
    let server = start().await;
    let (vpc_id, subnets) = vpc_with_subnets(&server, "10.44").await;
    let subnet_ids: Vec<String> = subnets.iter().map(|s| s.0.clone()).collect();
    let eks = server.eks_client().await;
    let asg = aws_sdk_autoscaling::Client::new(&server.aws_config().await);

    let cluster = eks
        .create_cluster()
        .name("placed")
        .role_arn("arn:aws:iam::000000000000:role/eks")
        .resources_vpc_config(
            VpcConfigRequest::builder()
                .set_subnet_ids(Some(subnet_ids.clone()))
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        cluster
            .cluster()
            .and_then(|c| c.resources_vpc_config())
            .and_then(|v| v.vpc_id()),
        Some(vpc_id.as_str())
    );

    let ng = eks
        .create_nodegroup()
        .cluster_name("placed")
        .nodegroup_name("workers")
        .node_role("arn:aws:iam::000000000000:role/node")
        .set_subnets(Some(subnet_ids.clone()))
        .scaling_config(
            NodegroupScalingConfig::builder()
                .min_size(1)
                .max_size(4)
                .desired_size(2)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let asg_name = ng
        .nodegroup()
        .unwrap()
        .resources()
        .unwrap()
        .auto_scaling_groups()[0]
        .name()
        .unwrap()
        .to_string();
    let groups = asg
        .describe_auto_scaling_groups()
        .auto_scaling_group_names(&asg_name)
        .send()
        .await
        .unwrap();
    let group = &groups.auto_scaling_groups()[0];
    assert_eq!(group.min_size(), Some(1));
    assert_eq!(group.max_size(), Some(4));
    assert_eq!(group.desired_capacity(), Some(2));
    assert_eq!(
        group.vpc_zone_identifier(),
        Some(subnet_ids.join(",").as_str())
    );
    // The group can be tagged the way the cluster-autoscaler setup does.
    asg.create_or_update_tags()
        .tags(
            aws_sdk_autoscaling::types::Tag::builder()
                .resource_id(&asg_name)
                .resource_type("auto-scaling-group")
                .key("k8s.io/cluster-autoscaler/node-template/label/team")
                .value("core")
                .propagate_at_launch(false)
                .build(),
        )
        .send()
        .await
        .unwrap();

    let addon = eks
        .create_addon()
        .cluster_name("placed")
        .addon_name("vpc-cni")
        .pod_identity_associations(
            AddonPodIdentityAssociations::builder()
                .service_account("aws-node")
                .role_arn("arn:aws:iam::000000000000:role/cni")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let addon = addon.addon().unwrap();
    let assoc_arn = addon.pod_identity_associations()[0].clone();
    let list = eks
        .list_pod_identity_associations()
        .cluster_name("placed")
        .send()
        .await
        .unwrap();
    let summary = list
        .associations()
        .iter()
        .find(|a| a.association_arn() == Some(assoc_arn.as_str()))
        .expect("the add-on's association is listed");
    let described = eks
        .describe_pod_identity_association()
        .cluster_name("placed")
        .association_id(summary.association_id().unwrap())
        .send()
        .await
        .unwrap();
    let assoc = described.association().unwrap();
    assert_eq!(assoc.service_account(), Some("aws-node"));
    assert_eq!(assoc.owner_arn(), addon.addon_arn());

    eks.delete_nodegroup()
        .cluster_name("placed")
        .nodegroup_name("workers")
        .send()
        .await
        .unwrap();
    let groups = asg
        .describe_auto_scaling_groups()
        .auto_scaling_group_names(&asg_name)
        .send()
        .await
        .unwrap();
    assert!(groups.auto_scaling_groups().is_empty());
}

#[tokio::test]
async fn lambda_vpc_config_and_opensearch_vpc_options_report_the_vpc() {
    let server = start().await;
    let (vpc_id, subnets) = vpc_with_subnets(&server, "10.45").await;
    let default_sg = default_sg_of(&server, &vpc_id).await;

    let lambda = server.lambda_client().await;
    let zip = {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        w.start_file("index.py", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"def handler(e, c):\n    return e\n").unwrap();
        w.finish().unwrap();
        buf.into_inner()
    };
    let created = lambda
        .create_function()
        .function_name("placed-fn")
        .runtime(aws_sdk_lambda::types::Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/test-role")
        .handler("index.handler")
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(aws_sdk_lambda::primitives::Blob::new(zip.clone()))
                .build(),
        )
        .vpc_config(
            aws_sdk_lambda::types::VpcConfig::builder()
                .subnet_ids(&subnets[0].0)
                .subnet_ids(&subnets[1].0)
                .security_group_ids(&default_sg)
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.vpc_config().and_then(|v| v.vpc_id()),
        Some(vpc_id.as_str())
    );
    let err = lambda
        .create_function()
        .function_name("ghost-fn")
        .runtime(aws_sdk_lambda::types::Runtime::Python312)
        .role("arn:aws:iam::123456789012:role/test-role")
        .handler("index.handler")
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(aws_sdk_lambda::primitives::Blob::new(zip))
                .build(),
        )
        .vpc_config(
            aws_sdk_lambda::types::VpcConfig::builder()
                .subnet_ids("subnet-0000000000000dead")
                .build(),
        )
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.into_service_error().meta().code(),
        Some("InvalidParameterValueException")
    );

    let out = server
        .aws_cli(&[
            "opensearch",
            "create-domain",
            "--domain-name",
            "placed-domain",
            "--vpc-options",
            &format!("SubnetIds={},{}", subnets[0].0, subnets[1].0),
        ])
        .await;
    assert!(out.success(), "{}", out.stderr_text());
    let vpc = &out.stdout_json()["DomainStatus"]["VPCOptions"];
    assert_eq!(vpc["VPCId"], vpc_id.as_str());
    assert_eq!(
        vpc["AvailabilityZones"],
        serde_json::json!(["us-east-1a", "us-east-1b"])
    );
    assert_eq!(vpc["SecurityGroupIds"], serde_json::json!([default_sg]));
}
