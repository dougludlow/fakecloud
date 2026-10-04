//! CloudFormation provisioner fidelity, read back through each service's SDK:
//!
//! - `Ref` on a DynamoDB table / EventBridge rule returns the name, as on AWS.
//! - Standalone VPC networking (`VPCGatewayAttachment`, `Route`,
//!   `SecurityGroupIngress`, `EIP`, `NatGateway`), `IAM::RolePolicy`,
//!   `IAM::Policy`, `Lambda::EventInvokeConfig`, `Scheduler::Schedule` and
//!   `DynamoDB::GlobalTable` exist in their services (they used to be recorded
//!   with no backing state).
//! - Stack updates reach the service like its own API: an SQS RedrivePolicy
//!   added by an update dead-letters messages, a Kinesis `ShardCount` change
//!   reshards without losing records, SNS topic tags and inline subscriptions
//!   follow the template.
//! - ECS services start with a PRIMARY deployment and their circuit breaker;
//!   Aurora members join their cluster on its port; a SAM state machine `Api`
//!   event is deployed to the implicit API's `Prod` stage.

mod helpers;

use aws_sdk_cloudformation::types::Capability;
use helpers::TestServer;

async fn wait_terminal(cfn: &aws_sdk_cloudformation::Client, stack: &str) -> String {
    helpers::wait_until(std::time::Duration::from_secs(60), || async {
        let out = cfn.describe_stacks().stack_name(stack).send().await.ok()?;
        let s = out.stacks().first()?;
        let status = s.stack_status()?.as_str().to_string();
        (!status.ends_with("IN_PROGRESS"))
            .then(|| format!("{status} {}", s.stack_status_reason().unwrap_or_default()))
    })
    .await
    .expect("stack reached a terminal status")
}

async fn deploy(server: &TestServer, stack: &str, template: &str) {
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name(stack)
        .template_body(template)
        .capabilities(Capability::CapabilityNamedIam)
        .send()
        .await
        .expect("create_stack");
    let status = wait_terminal(&cfn, stack).await;
    assert!(status.starts_with("CREATE_COMPLETE"), "{status}");
}

async fn redeploy(server: &TestServer, stack: &str, template: &str) {
    let cfn = server.cloudformation_client().await;
    cfn.update_stack()
        .stack_name(stack)
        .template_body(template)
        .capabilities(Capability::CapabilityNamedIam)
        .send()
        .await
        .expect("update_stack");
    let status = wait_terminal(&cfn, stack).await;
    assert!(status.starts_with("UPDATE_COMPLETE"), "{status}");
}

async fn outputs(server: &TestServer, stack: &str) -> std::collections::HashMap<String, String> {
    let cfn = server.cloudformation_client().await;
    let out = cfn
        .describe_stacks()
        .stack_name(stack)
        .send()
        .await
        .expect("describe_stacks");
    out.stacks()[0]
        .outputs()
        .iter()
        .map(|o| {
            (
                o.output_key().unwrap_or_default().to_string(),
                o.output_value().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

const BACKED_TEMPLATE: &str = r#"{
  "Resources": {
    "Table": {
      "Type": "AWS::DynamoDB::Table",
      "Properties": {
        "TableName": "fid-table",
        "BillingMode": "PAY_PER_REQUEST",
        "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}],
        "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}]
      }
    },
    "Rule": {
      "Type": "AWS::Events::Rule",
      "Properties": {"Name": "fid-rule", "ScheduleExpression": "rate(1 hour)"}
    },
    "Vpc": {"Type": "AWS::EC2::VPC", "Properties": {"CidrBlock": "10.42.0.0/16"}},
    "Subnet": {"Type": "AWS::EC2::Subnet", "Properties": {"VpcId": {"Ref": "Vpc"}, "CidrBlock": "10.42.1.0/24"}},
    "Igw": {"Type": "AWS::EC2::InternetGateway"},
    "Attach": {
      "Type": "AWS::EC2::VPCGatewayAttachment",
      "Properties": {"VpcId": {"Ref": "Vpc"}, "InternetGatewayId": {"Ref": "Igw"}}
    },
    "Rtb": {"Type": "AWS::EC2::RouteTable", "Properties": {"VpcId": {"Ref": "Vpc"}}},
    "DefaultRoute": {
      "Type": "AWS::EC2::Route",
      "DependsOn": "Attach",
      "Properties": {"RouteTableId": {"Ref": "Rtb"}, "DestinationCidrBlock": "0.0.0.0/0", "GatewayId": {"Ref": "Igw"}}
    },
    "Sg": {"Type": "AWS::EC2::SecurityGroup", "Properties": {"GroupDescription": "web", "VpcId": {"Ref": "Vpc"}}},
    "Https": {
      "Type": "AWS::EC2::SecurityGroupIngress",
      "Properties": {"GroupId": {"Ref": "Sg"}, "IpProtocol": "tcp", "FromPort": 443, "ToPort": 443, "CidrIp": "0.0.0.0/0"}
    },
    "Eip": {"Type": "AWS::EC2::EIP", "Properties": {"Domain": "vpc"}},
    "Nat": {
      "Type": "AWS::EC2::NatGateway",
      "Properties": {"SubnetId": {"Ref": "Subnet"}, "AllocationId": {"Fn::GetAtt": ["Eip", "AllocationId"]}}
    },
    "Role": {
      "Type": "AWS::IAM::Role",
      "Properties": {
        "RoleName": "fid-role",
        "AssumeRolePolicyDocument": {"Version": "2012-10-17", "Statement": [
          {"Effect": "Allow", "Principal": {"Service": "lambda.amazonaws.com"}, "Action": "sts:AssumeRole"}
        ]}
      }
    },
    "RolePolicy": {
      "Type": "AWS::IAM::RolePolicy",
      "Properties": {
        "RoleName": {"Ref": "Role"},
        "PolicyName": "role-policy",
        "PolicyDocument": {"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "sqs:SendMessage", "Resource": "*"}]}
      }
    },
    "Policy": {
      "Type": "AWS::IAM::Policy",
      "Properties": {
        "PolicyName": "default-policy",
        "Roles": [{"Ref": "Role"}],
        "PolicyDocument": {"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": "dynamodb:GetItem", "Resource": "*"}]}
      }
    },
    "Fn": {
      "Type": "AWS::Lambda::Function",
      "Properties": {
        "FunctionName": "fid-fn",
        "Runtime": "python3.12",
        "Handler": "index.handler",
        "Role": {"Fn::GetAtt": ["Role", "Arn"]},
        "ReservedConcurrentExecutions": 7,
        "Code": {"ZipFile": "def handler(e, c):\n    return e\n"}
      }
    },
    "FnAsync": {
      "Type": "AWS::Lambda::EventInvokeConfig",
      "Properties": {"FunctionName": {"Ref": "Fn"}, "Qualifier": "$LATEST", "MaximumRetryAttempts": 1}
    },
    "Target": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-target"}},
    "Schedule": {
      "Type": "AWS::Scheduler::Schedule",
      "Properties": {
        "Name": "fid-schedule",
        "ScheduleExpression": "rate(5 minutes)",
        "FlexibleTimeWindow": {"Mode": "OFF"},
        "Target": {"Arn": {"Fn::GetAtt": ["Target", "Arn"]}, "RoleArn": {"Fn::GetAtt": ["Role", "Arn"]}}
      }
    },
    "Global": {
      "Type": "AWS::DynamoDB::GlobalTable",
      "Properties": {
        "TableName": "fid-global",
        "BillingMode": "PAY_PER_REQUEST",
        "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}],
        "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}],
        "Replicas": [{"Region": "us-east-1"}, {"Region": "eu-west-1"}]
      }
    }
  },
  "Outputs": {
    "TableRef": {"Value": {"Ref": "Table"}},
    "RuleRef": {"Value": {"Ref": "Rule"}},
    "EipRef": {"Value": {"Ref": "Eip"}},
    "NatRef": {"Value": {"Ref": "Nat"}},
    "IngressRef": {"Value": {"Ref": "Https"}}
  }
}"#;

#[tokio::test]
async fn cfn_refs_and_previously_unbacked_types_reach_their_services() {
    let server = TestServer::start().await;
    deploy(&server, "fid-backed", BACKED_TEMPLATE).await;
    let out = outputs(&server, "fid-backed").await;
    assert_eq!(out["TableRef"], "fid-table");
    assert_eq!(out["RuleRef"], "fid-rule");

    let ec2 = aws_sdk_ec2::Client::new(&server.aws_config().await);
    let rtbs = ec2.describe_route_tables().send().await.unwrap();
    assert!(
        rtbs.route_tables().iter().any(|t| t
            .routes()
            .iter()
            .any(|r| r.destination_cidr_block() == Some("0.0.0.0/0"))),
        "default route missing"
    );
    let igws = ec2.describe_internet_gateways().send().await.unwrap();
    assert!(igws
        .internet_gateways()
        .iter()
        .any(|g| !g.attachments().is_empty()));
    let eips = ec2.describe_addresses().send().await.unwrap();
    assert!(eips
        .addresses()
        .iter()
        .any(|a| a.public_ip() == Some(out["EipRef"].as_str())));
    let nats = ec2
        .describe_nat_gateways()
        .nat_gateway_ids(&out["NatRef"])
        .send()
        .await
        .unwrap();
    assert_eq!(nats.nat_gateways().len(), 1);
    let rules = ec2
        .describe_security_group_rules()
        .security_group_rule_ids(&out["IngressRef"])
        .send()
        .await
        .unwrap();
    assert_eq!(rules.security_group_rules()[0].from_port(), Some(443));

    let iam = server.iam_client().await;
    let inline = iam
        .list_role_policies()
        .role_name("fid-role")
        .send()
        .await
        .unwrap();
    let names = inline.policy_names();
    assert!(names.iter().any(|n| n == "role-policy"), "{names:?}");
    assert!(names.iter().any(|n| n == "default-policy"), "{names:?}");

    let lambda = aws_sdk_lambda::Client::new(&server.aws_config().await);
    let conc = lambda
        .get_function_concurrency()
        .function_name("fid-fn")
        .send()
        .await
        .unwrap();
    assert_eq!(conc.reserved_concurrent_executions(), Some(7));
    let eic = lambda
        .get_function_event_invoke_config()
        .function_name("fid-fn")
        .qualifier("$LATEST")
        .send()
        .await
        .unwrap();
    assert_eq!(eic.maximum_retry_attempts(), Some(1));

    let scheduler = aws_sdk_scheduler::Client::new(&server.aws_config().await);
    let sched = scheduler
        .get_schedule()
        .name("fid-schedule")
        .send()
        .await
        .unwrap();
    assert_eq!(sched.schedule_expression(), Some("rate(5 minutes)"));

    let ddb = aws_sdk_dynamodb::Client::new(&server.aws_config().await);
    let gt = ddb
        .describe_global_table()
        .global_table_name("fid-global")
        .send()
        .await
        .unwrap();
    assert_eq!(
        gt.global_table_description()
            .unwrap()
            .replication_group()
            .len(),
        2
    );

    // Deleting the stack removes the backing state too.
    let cfn = server.cloudformation_client().await;
    cfn.delete_stack()
        .stack_name("fid-backed")
        .send()
        .await
        .unwrap();
    helpers::wait_until(std::time::Duration::from_secs(30), || async {
        scheduler
            .get_schedule()
            .name("fid-schedule")
            .send()
            .await
            .is_err()
            .then_some(())
    })
    .await
    .expect("schedule removed with the stack");
    let inline = iam.list_role_policies().role_name("fid-role").send().await;
    assert!(inline.is_err() || inline.unwrap().policy_names().is_empty());
}

const SQS_V1: &str = r#"{
  "Resources": {
    "Dlq": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-dlq"}},
    "Work": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-work", "VisibilityTimeout": 0}}
  }
}"#;

const SQS_V2: &str = r#"{
  "Resources": {
    "Dlq": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-dlq"}},
    "Work": {
      "Type": "AWS::SQS::Queue",
      "Properties": {
        "QueueName": "fid-work",
        "VisibilityTimeout": 0,
        "RedrivePolicy": {"deadLetterTargetArn": {"Fn::GetAtt": ["Dlq", "Arn"]}, "maxReceiveCount": 1}
      }
    }
  }
}"#;

#[tokio::test]
async fn cfn_sqs_dlq_added_by_an_update_receives_failed_messages() {
    let server = TestServer::start().await;
    deploy(&server, "fid-sqs", SQS_V1).await;
    redeploy(&server, "fid-sqs", SQS_V2).await;

    let sqs = server.sqs_client().await;
    let work = sqs
        .get_queue_url()
        .queue_name("fid-work")
        .send()
        .await
        .unwrap()
        .queue_url()
        .unwrap()
        .to_string();
    let dlq = sqs
        .get_queue_url()
        .queue_name("fid-dlq")
        .send()
        .await
        .unwrap()
        .queue_url()
        .unwrap()
        .to_string();
    sqs.send_message()
        .queue_url(&work)
        .message_body("poison")
        .send()
        .await
        .unwrap();
    // First receive uses the one allowed attempt; the next moves it to the DLQ.
    for _ in 0..2 {
        sqs.receive_message().queue_url(&work).send().await.unwrap();
    }
    let got = helpers::wait_until(std::time::Duration::from_secs(10), || async {
        let r = sqs.receive_message().queue_url(&dlq).send().await.ok()?;
        r.messages()
            .iter()
            .any(|m| m.body() == Some("poison"))
            .then_some(())
    })
    .await;
    assert!(
        got.is_some(),
        "message never reached the DLQ added by the update"
    );
}

const KINESIS_V1: &str = r#"{"Resources": {"S": {"Type": "AWS::Kinesis::Stream", "Properties": {"Name": "fid-stream", "ShardCount": 1, "Tags": [{"Key": "env", "Value": "dev"}]}}}}"#;
const KINESIS_V2: &str = r#"{"Resources": {"S": {"Type": "AWS::Kinesis::Stream", "Properties": {"Name": "fid-stream", "ShardCount": 2, "StreamEncryption": {"EncryptionType": "KMS", "KeyId": "alias/aws/kinesis"}}}}}"#;

#[tokio::test]
async fn cfn_kinesis_shard_count_update_reshards_without_losing_records() {
    let server = TestServer::start().await;
    deploy(&server, "fid-kinesis", KINESIS_V1).await;
    let kinesis = aws_sdk_kinesis::Client::new(&server.aws_config().await);
    let tags = kinesis
        .list_tags_for_stream()
        .stream_name("fid-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(tags.tags()[0].key(), "env");
    kinesis
        .put_record()
        .stream_name("fid-stream")
        .partition_key("pk")
        .data(aws_sdk_kinesis::primitives::Blob::new(b"before".to_vec()))
        .send()
        .await
        .unwrap();

    redeploy(&server, "fid-kinesis", KINESIS_V2).await;

    let shards = kinesis
        .list_shards()
        .stream_name("fid-stream")
        .send()
        .await
        .unwrap();
    let shards = shards.shards();
    assert_eq!(shards.len(), 3, "parent + two children");
    let parent = shards[0].shard_id().to_string();
    assert!(shards[1..]
        .iter()
        .all(|s| s.parent_shard_id() == Some(parent.as_str())));
    let it = kinesis
        .get_shard_iterator()
        .stream_name("fid-stream")
        .shard_id(&parent)
        .shard_iterator_type(aws_sdk_kinesis::types::ShardIteratorType::TrimHorizon)
        .send()
        .await
        .unwrap();
    let recs = kinesis
        .get_records()
        .shard_iterator(it.shard_iterator().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(recs.records()[0].data().as_ref(), b"before");
    let summary = kinesis
        .describe_stream_summary()
        .stream_name("fid-stream")
        .send()
        .await
        .unwrap();
    let d = summary.stream_description_summary().unwrap();
    assert_eq!(d.encryption_type().map(|e| e.as_str()), Some("KMS"));
    let tags = kinesis
        .list_tags_for_stream()
        .stream_name("fid-stream")
        .send()
        .await
        .unwrap();
    assert!(tags.tags().is_empty(), "dropped Tags are removed");
}

const SNS_V1: &str = r#"{
  "Resources": {
    "Q1": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-sub-1"}},
    "Q2": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-sub-2"}},
    "Topic": {
      "Type": "AWS::SNS::Topic",
      "Properties": {
        "TopicName": "fid-topic",
        "Tags": [{"Key": "v", "Value": "1"}],
        "Subscription": [{"Protocol": "sqs", "Endpoint": {"Fn::GetAtt": ["Q1", "Arn"]}}]
      }
    }
  }
}"#;

const SNS_V2: &str = r#"{
  "Resources": {
    "Q1": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-sub-1"}},
    "Q2": {"Type": "AWS::SQS::Queue", "Properties": {"QueueName": "fid-sub-2"}},
    "Topic": {
      "Type": "AWS::SNS::Topic",
      "Properties": {
        "TopicName": "fid-topic",
        "Tags": [{"Key": "v", "Value": "2"}],
        "Subscription": [{"Protocol": "sqs", "Endpoint": {"Fn::GetAtt": ["Q2", "Arn"]}}]
      }
    }
  }
}"#;

#[tokio::test]
async fn cfn_sns_topic_update_applies_tags_and_inline_subscriptions() {
    let server = TestServer::start().await;
    deploy(&server, "fid-sns", SNS_V1).await;
    redeploy(&server, "fid-sns", SNS_V2).await;
    let sns = server.sns_client().await;
    let topics = sns.list_topics().send().await.unwrap();
    let topic_arn = topics
        .topics()
        .iter()
        .filter_map(|t| t.topic_arn())
        .find(|a| a.ends_with(":fid-topic"))
        .expect("topic")
        .to_string();
    let topic_arn = topic_arn.as_str();
    let tags = sns
        .list_tags_for_resource()
        .resource_arn(topic_arn)
        .send()
        .await
        .unwrap();
    assert_eq!(tags.tags().len(), 1);
    assert_eq!(tags.tags()[0].value(), "2");
    let subs = sns
        .list_subscriptions_by_topic()
        .topic_arn(topic_arn)
        .send()
        .await
        .unwrap();
    let endpoints: Vec<&str> = subs
        .subscriptions()
        .iter()
        .filter_map(|s| s.endpoint())
        .collect();
    assert_eq!(endpoints.len(), 1, "{endpoints:?}");
    assert!(endpoints[0].ends_with(":fid-sub-2"), "{endpoints:?}");
}

const ECS_TEMPLATE: &str = r#"{
  "Resources": {
    "Cluster": {"Type": "AWS::ECS::Cluster", "Properties": {"ClusterName": "fid-cluster"}},
    "Td": {
      "Type": "AWS::ECS::TaskDefinition",
      "Properties": {"Family": "fid-web", "ContainerDefinitions": [{"Name": "app", "Image": "nginx", "Essential": true}]}
    },
    "Svc": {
      "Type": "AWS::ECS::Service",
      "Properties": {
        "ServiceName": "fid-svc",
        "Cluster": {"Ref": "Cluster"},
        "TaskDefinition": {"Ref": "Td"},
        "DesiredCount": 0,
        "DeploymentConfiguration": {"DeploymentCircuitBreaker": {"Enable": true, "Rollback": false}}
      }
    }
  }
}"#;

#[tokio::test]
async fn cfn_ecs_service_has_a_primary_deployment_and_circuit_breaker() {
    let server = TestServer::start().await;
    deploy(&server, "fid-ecs", ECS_TEMPLATE).await;
    let ecs = aws_sdk_ecs::Client::new(&server.aws_config().await);
    let out = ecs
        .describe_services()
        .cluster("fid-cluster")
        .services("fid-svc")
        .send()
        .await
        .unwrap();
    let svc = &out.services()[0];
    let deployments = svc.deployments();
    assert_eq!(deployments.len(), 1);
    assert_eq!(deployments[0].status(), Some("PRIMARY"));
    let cb = svc
        .deployment_configuration()
        .and_then(|d| d.deployment_circuit_breaker())
        .expect("circuit breaker");
    assert!(cb.enable());
    assert!(!cb.rollback());
}

const AURORA_TEMPLATE: &str = r#"{
  "Resources": {
    "Cluster": {
      "Type": "AWS::RDS::DBCluster",
      "Properties": {
        "DBClusterIdentifier": "fid-aurora",
        "Engine": "aurora-mysql",
        "MasterUsername": "admin",
        "MasterUserPassword": "fid-secret-pw"
      }
    },
    "Writer": {
      "Type": "AWS::RDS::DBInstance",
      "Properties": {
        "DBInstanceIdentifier": "fid-aurora-1",
        "DBInstanceClass": "db.r6g.large",
        "Engine": "aurora-mysql",
        "DBClusterIdentifier": {"Ref": "Cluster"}
      }
    }
  },
  "Outputs": {"Port": {"Value": {"Fn::GetAtt": ["Cluster", "Endpoint.Port"]}}}
}"#;

#[tokio::test]
async fn cfn_aurora_member_joins_its_cluster_on_the_engine_port() {
    let server = TestServer::start().await;
    deploy(&server, "fid-aurora", AURORA_TEMPLATE).await;
    assert_eq!(outputs(&server, "fid-aurora").await["Port"], "3306");
    let rds = aws_sdk_rds::Client::new(&server.aws_config().await);
    let clusters = rds
        .describe_db_clusters()
        .db_cluster_identifier("fid-aurora")
        .send()
        .await
        .unwrap();
    let c = &clusters.db_clusters()[0];
    assert_eq!(c.port(), Some(3306));
    assert!(
        c.endpoint().unwrap().contains(".cluster-")
            && c.endpoint()
                .unwrap()
                .ends_with(".us-east-1.rds.amazonaws.com"),
        "{:?}",
        c.endpoint()
    );
    let members = c.db_cluster_members();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].db_instance_identifier(), Some("fid-aurora-1"));
    assert_eq!(members[0].is_cluster_writer(), Some(true));
}

const SAM_SFN_API: &str = r#"{
  "Transform": "AWS::Serverless-2016-10-31",
  "Resources": {
    "Machine": {
      "Type": "AWS::Serverless::StateMachine",
      "Properties": {
        "Role": "arn:aws:iam::123456789012:role/sfn-role",
        "Definition": {"StartAt": "Done", "States": {"Done": {"Type": "Succeed"}}},
        "Events": {"Start": {"Type": "Api", "Properties": {"Path": "/start", "Method": "post"}}}
      }
    }
  },
  "Outputs": {"ApiId": {"Value": {"Ref": "ServerlessRestApi"}}}
}"#;

#[tokio::test]
async fn cfn_sam_state_machine_api_event_is_deployed_to_prod() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name("fid-sam-sfn")
        .template_body(SAM_SFN_API)
        .capabilities(Capability::CapabilityIam)
        .capabilities(Capability::CapabilityAutoExpand)
        .send()
        .await
        .expect("create_stack");
    let status = wait_terminal(&cfn, "fid-sam-sfn").await;
    assert!(status.starts_with("CREATE_COMPLETE"), "{status}");
    let api_id = outputs(&server, "fid-sam-sfn").await["ApiId"].clone();
    let apigw = aws_sdk_apigateway::Client::new(&server.aws_config().await);
    let stages = apigw
        .get_stages()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    assert!(
        stages.item().iter().any(|s| s.stage_name() == Some("Prod")),
        "{:?}",
        stages.item()
    );
    let resources = apigw
        .get_resources()
        .rest_api_id(&api_id)
        .send()
        .await
        .unwrap();
    assert!(resources.items().iter().any(|r| r.path() == Some("/start")));
}

const BATCH_TEMPLATE: &str = r#"{
  "Resources": {
    "Licenses": {
      "Type": "AWS::Batch::ConsumableResource",
      "Properties": {"ConsumableResourceName": "fid-licenses", "TotalQuantity": 3, "ResourceType": "REPLENISHABLE"}
    }
  },
  "Outputs": {"Arn": {"Value": {"Ref": "Licenses"}}}
}"#;

#[tokio::test]
async fn cfn_batch_consumable_resource_is_real() {
    let server = TestServer::start().await;
    deploy(&server, "fid-batch", BATCH_TEMPLATE).await;
    let arn = outputs(&server, "fid-batch").await["Arn"].clone();
    assert!(arn.contains(":consumable-resource/fid-licenses"), "{arn}");
    let batch = aws_sdk_batch::Client::new(&server.aws_config().await);
    let out = batch
        .describe_consumable_resource()
        .consumable_resource("fid-licenses")
        .send()
        .await
        .unwrap();
    assert_eq!(out.total_quantity(), Some(3));
}
