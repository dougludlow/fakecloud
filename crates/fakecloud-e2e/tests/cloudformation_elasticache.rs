//! CloudFormation provisioner for ElastiCache metadata resource types
//! (ParameterGroup, SubnetGroup, SecurityGroup, User, UserGroup).

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "PG": {
      "Type": "AWS::ElastiCache::ParameterGroup",
      "Properties": {
        "CacheParameterGroupName": "cfn-ec-pg",
        "CacheParameterGroupFamily": "redis7",
        "Description": "test pg"
      }
    },
    "SubnetGroup": {
      "Type": "AWS::ElastiCache::SubnetGroup",
      "Properties": {
        "CacheSubnetGroupName": "cfn-ec-subnets",
        "Description": "test subnets",
        "SubnetIds": ["subnet-aaa", "subnet-bbb"]
      }
    },
    "SG": {
      "Type": "AWS::ElastiCache::SecurityGroup",
      "Properties": {
        "Description": "test ec sg"
      }
    },
    "User": {
      "Type": "AWS::ElastiCache::User",
      "Properties": {
        "UserId": "cfn-ec-user",
        "UserName": "cfn-user",
        "Engine": "redis",
        "AccessString": "on ~* +@all"
      }
    },
    "UserGroup": {
      "Type": "AWS::ElastiCache::UserGroup",
      "Properties": {
        "UserGroupId": "cfn-ec-usergroup",
        "Engine": "redis",
        "UserIds": [{"Ref": "User"}]
      }
    }
  },
  "Outputs": {
    "PgName": {"Value": {"Ref": "PG"}},
    "PgArn": {"Value": {"Fn::GetAtt": ["PG", "Arn"]}},
    "SubnetGroupName": {"Value": {"Ref": "SubnetGroup"}},
    "SgName": {"Value": {"Ref": "SG"}},
    "UserId": {"Value": {"Ref": "User"}},
    "UserGroupId": {"Value": {"Ref": "UserGroup"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_elasticache_metadata() {
    let server = TestServer::start().await;
    let vpc_subnets = server.default_subnet_ids().await;
    let cfn = server.cloudformation_client().await;
    let ec = aws_sdk_elasticache::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("ec-stack")
        .template_body(with_default_subnets(TEMPLATE, &vpc_subnets))
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("ec-stack")
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
    assert_eq!(outs.get("PgName").map(|s| s.as_str()), Some("cfn-ec-pg"));
    assert!(outs
        .get("PgArn")
        .unwrap()
        .contains(":parametergroup:cfn-ec-pg"));
    assert_eq!(
        outs.get("SubnetGroupName").map(|s| s.as_str()),
        Some("cfn-ec-subnets")
    );
    assert_eq!(outs.get("UserId").map(|s| s.as_str()), Some("cfn-ec-user"));
    assert_eq!(
        outs.get("UserGroupId").map(|s| s.as_str()),
        Some("cfn-ec-usergroup")
    );

    // Verify SDK reads.
    let pgs = ec
        .describe_cache_parameter_groups()
        .cache_parameter_group_name("cfn-ec-pg")
        .send()
        .await
        .expect("describe_cache_parameter_groups");
    assert_eq!(
        pgs.cache_parameter_groups()
            .first()
            .and_then(|g| g.cache_parameter_group_name()),
        Some("cfn-ec-pg")
    );

    let subnet_groups = ec
        .describe_cache_subnet_groups()
        .cache_subnet_group_name("cfn-ec-subnets")
        .send()
        .await
        .expect("describe_cache_subnet_groups");
    assert_eq!(
        subnet_groups
            .cache_subnet_groups()
            .first()
            .and_then(|g| g.cache_subnet_group_name()),
        Some("cfn-ec-subnets")
    );

    let users = ec
        .describe_users()
        .user_id("cfn-ec-user")
        .send()
        .await
        .expect("describe_users");
    assert_eq!(
        users.users().first().and_then(|u| u.user_id()),
        Some("cfn-ec-user")
    );

    let user_groups = ec
        .describe_user_groups()
        .user_group_id("cfn-ec-usergroup")
        .send()
        .await
        .expect("describe_user_groups");
    assert_eq!(
        user_groups
            .user_groups()
            .first()
            .and_then(|g| g.user_group_id()),
        Some("cfn-ec-usergroup")
    );

    cfn.delete_stack()
        .stack_name("ec-stack")
        .send()
        .await
        .expect("delete_stack");

    let after = ec.describe_users().user_id("cfn-ec-user").send().await;
    assert!(after.is_err(), "user should be gone after stack deletion");
}

/// The template with its `subnet-aaa` / `subnet-bbb` placeholders replaced by
/// the account's default subnets, which the resources resolve in EC2.
fn with_default_subnets(template: &str, subnets: &[String]) -> String {
    template
        .replace("subnet-aaa", &subnets[0])
        .replace("subnet-bbb", &subnets[1])
}
