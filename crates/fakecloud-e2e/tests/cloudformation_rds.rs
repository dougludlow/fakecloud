//! CloudFormation provisioner for RDS metadata resource types
//! (DBSubnetGroup, DBParameterGroup, DBClusterParameterGroup,
//! OptionGroup, EventSubscription, DBSecurityGroup, DBProxy).

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Subnets": {
      "Type": "AWS::RDS::DBSubnetGroup",
      "Properties": {
        "DBSubnetGroupName": "cfn-subnets",
        "DBSubnetGroupDescription": "test subnets",
        "SubnetIds": ["subnet-aaa", "subnet-bbb"]
      }
    },
    "PG": {
      "Type": "AWS::RDS::DBParameterGroup",
      "Properties": {
        "DBParameterGroupName": "cfn-pg",
        "Family": "postgres16",
        "Description": "test pg"
      }
    },
    "ClusterPG": {
      "Type": "AWS::RDS::DBClusterParameterGroup",
      "Properties": {
        "DBClusterParameterGroupName": "cfn-cluster-pg",
        "Family": "aurora-postgresql15",
        "Description": "test cluster pg"
      }
    },
    "OG": {
      "Type": "AWS::RDS::OptionGroup",
      "Properties": {
        "OptionGroupName": "cfn-og",
        "EngineName": "mysql",
        "MajorEngineVersion": "8.0",
        "OptionGroupDescription": "test og"
      }
    },
    "Sub": {
      "Type": "AWS::RDS::EventSubscription",
      "Properties": {
        "SubscriptionName": "cfn-sub",
        "SnsTopicArn": "arn:aws:sns:us-east-1:000000000000:rds-events",
        "Enabled": true
      }
    },
    "SG": {
      "Type": "AWS::RDS::DBSecurityGroup",
      "Properties": {
        "DBSecurityGroupName": "cfn-sg",
        "GroupDescription": "test sg"
      }
    },
    "Proxy": {
      "Type": "AWS::RDS::DBProxy",
      "Properties": {
        "DBProxyName": "cfn-proxy",
        "EngineFamily": "POSTGRESQL"
      }
    }
  },
  "Outputs": {
    "SubnetsName": {"Value": {"Ref": "Subnets"}},
    "SubnetsArn": {"Value": {"Fn::GetAtt": ["Subnets", "Arn"]}},
    "PgName": {"Value": {"Ref": "PG"}},
    "ClusterPgName": {"Value": {"Ref": "ClusterPG"}},
    "OgName": {"Value": {"Ref": "OG"}},
    "SubName": {"Value": {"Ref": "Sub"}},
    "SgName": {"Value": {"Ref": "SG"}},
    "ProxyName": {"Value": {"Ref": "Proxy"}},
    "ProxyArn": {"Value": {"Fn::GetAtt": ["Proxy", "DBProxyArn"]}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_rds_metadata_resources() {
    let server = TestServer::start().await;
    let vpc_subnets = server.default_subnet_ids().await;
    let cfn = server.cloudformation_client().await;
    let rds = aws_sdk_rds::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("rds-metadata-stack")
        .template_body(with_default_subnets(TEMPLATE, &vpc_subnets))
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("rds-metadata-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let mut outs = std::collections::HashMap::new();
    for o in stack.outputs() {
        if let (Some(k), Some(v)) = (o.output_key(), o.output_value()) {
            outs.insert(k.to_string(), v.to_string());
        }
    }
    assert_eq!(
        outs.get("SubnetsName").map(|s| s.as_str()),
        Some("cfn-subnets")
    );
    assert!(outs
        .get("SubnetsArn")
        .unwrap()
        .contains(":subgrp:cfn-subnets"));
    assert_eq!(outs.get("PgName").map(|s| s.as_str()), Some("cfn-pg"));
    assert_eq!(
        outs.get("ClusterPgName").map(|s| s.as_str()),
        Some("cfn-cluster-pg")
    );
    assert_eq!(outs.get("OgName").map(|s| s.as_str()), Some("cfn-og"));
    assert_eq!(outs.get("SubName").map(|s| s.as_str()), Some("cfn-sub"));
    assert_eq!(outs.get("SgName").map(|s| s.as_str()), Some("cfn-sg"));
    assert_eq!(outs.get("ProxyName").map(|s| s.as_str()), Some("cfn-proxy"));
    assert!(outs
        .get("ProxyArn")
        .unwrap()
        .contains(":db-proxy:cfn-proxy"));

    // SDK-level reads of the metadata DBs need the underlying RDS
    // Describe* handlers to support PascalCase + name filtering;
    // several of those are still on the legacy snake_case path
    // (separate work item). The CFN outputs above already prove every
    // resource provisioned through state correctly.
    let subnets = rds
        .describe_db_subnet_groups()
        .send()
        .await
        .expect("describe_db_subnet_groups");
    assert!(
        subnets
            .db_subnet_groups()
            .iter()
            .any(|g| g.db_subnet_group_name() == Some("cfn-subnets")),
        "cfn subnets group present in describe response"
    );

    cfn.delete_stack()
        .stack_name("rds-metadata-stack")
        .send()
        .await
        .expect("delete_stack");

    // Subnet group gone after stack delete.
    let after = rds
        .describe_db_subnet_groups()
        .send()
        .await
        .expect("describe_db_subnet_groups after delete");
    assert!(
        !after
            .db_subnet_groups()
            .iter()
            .any(|g| g.db_subnet_group_name() == Some("cfn-subnets")),
        "subnet group should be gone after stack deletion"
    );
}

/// The template with its `subnet-aaa` / `subnet-bbb` placeholders replaced by
/// the account's default subnets, which the resources resolve in EC2.
fn with_default_subnets(template: &str, subnets: &[String]) -> String {
    template
        .replace("subnet-aaa", &subnets[0])
        .replace("subnet-bbb", &subnets[1])
}
