//! CloudFormation provisioner for ELBv2 LB + TargetGroup + Listener + ListenerRule.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Web": {
      "Type": "AWS::ElasticLoadBalancingV2::LoadBalancer",
      "Properties": {
        "Name": "cfn-web",
        "Scheme": "internet-facing",
        "Type": "application",
        "Subnets": ["subnet-aaa", "subnet-bbb"],
        "SecurityGroups": ["sg-deadbeef"]
      }
    },
    "ApiTg": {
      "Type": "AWS::ElasticLoadBalancingV2::TargetGroup",
      "Properties": {
        "Name": "cfn-api",
        "Protocol": "HTTP",
        "Port": 8080,
        "VpcId": "vpc-1234",
        "TargetType": "ip",
        "HealthCheckPath": "/healthz"
      }
    },
    "WebListener": {
      "Type": "AWS::ElasticLoadBalancingV2::Listener",
      "Properties": {
        "LoadBalancerArn": {"Ref": "Web"},
        "Port": 80,
        "Protocol": "HTTP",
        "DefaultActions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "ApiTg"}}
        ]
      }
    },
    "WebRule": {
      "Type": "AWS::ElasticLoadBalancingV2::ListenerRule",
      "Properties": {
        "ListenerArn": {"Ref": "WebListener"},
        "Priority": 10,
        "Conditions": [
          {"Field": "host-header", "Values": ["api.example.com"]}
        ],
        "Actions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "ApiTg"}}
        ]
      }
    }
  },
  "Outputs": {
    "LbArn": {"Value": {"Ref": "Web"}},
    "LbDns": {"Value": {"Fn::GetAtt": ["Web", "DNSName"]}},
    "TgArn": {"Value": {"Ref": "ApiTg"}},
    "ListenerArn": {"Value": {"Ref": "WebListener"}},
    "RuleArn": {"Value": {"Ref": "WebRule"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_elbv2_lb_tg_listener_rule() {
    let server = TestServer::start().await;
    let vpc = vpc_fixture(&server).await;
    let cfn = server.cloudformation_client().await;
    let elb = server.elbv2_client().await;

    cfn.create_stack()
        .stack_name("elbv2-stack")
        .template_body(with_vpc_ids(TEMPLATE, &vpc))
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("elbv2-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let mut lb_arn = None;
    let mut tg_arn = None;
    let mut listener_arn = None;
    let mut rule_arn = None;
    for o in stack.outputs() {
        match o.output_key() {
            Some("LbArn") => lb_arn = o.output_value().map(|s| s.to_string()),
            Some("TgArn") => tg_arn = o.output_value().map(|s| s.to_string()),
            Some("ListenerArn") => listener_arn = o.output_value().map(|s| s.to_string()),
            Some("RuleArn") => rule_arn = o.output_value().map(|s| s.to_string()),
            _ => {}
        }
    }
    let lb_arn = lb_arn.expect("LbArn");
    let tg_arn = tg_arn.expect("TgArn");
    let listener_arn = listener_arn.expect("ListenerArn");
    let rule_arn = rule_arn.expect("RuleArn");

    let lbs = elb
        .describe_load_balancers()
        .load_balancer_arns(&lb_arn)
        .send()
        .await
        .expect("describe_load_balancers");
    let lb = lbs.load_balancers().first().expect("lb");
    assert_eq!(lb.load_balancer_name(), Some("cfn-web"));

    let tgs = elb
        .describe_target_groups()
        .target_group_arns(&tg_arn)
        .send()
        .await
        .expect("describe_target_groups");
    let tg = tgs.target_groups().first().expect("tg");
    assert_eq!(tg.target_group_name(), Some("cfn-api"));
    assert_eq!(tg.port(), Some(8080));

    let listeners = elb
        .describe_listeners()
        .load_balancer_arn(&lb_arn)
        .send()
        .await
        .expect("describe_listeners");
    assert!(listeners
        .listeners()
        .iter()
        .any(|l| l.listener_arn() == Some(&listener_arn)));

    let rules = elb
        .describe_rules()
        .listener_arn(&listener_arn)
        .send()
        .await
        .expect("describe_rules");
    assert!(
        rules
            .rules()
            .iter()
            .any(|r| r.rule_arn() == Some(&rule_arn)),
        "rule should exist on listener"
    );

    cfn.delete_stack()
        .stack_name("elbv2-stack")
        .send()
        .await
        .expect("delete_stack");

    let after = elb
        .describe_load_balancers()
        .load_balancer_arns(&lb_arn)
        .send()
        .await;
    assert!(after.is_err(), "lb should be gone after stack deletion");
}

const EXTRAS_TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Web": {
      "Type": "AWS::ElasticLoadBalancingV2::LoadBalancer",
      "Properties": {
        "Name": "cfn-extras-web",
        "Scheme": "internet-facing",
        "Type": "application",
        "Subnets": ["subnet-aaa", "subnet-bbb"]
      }
    },
    "ApiTg": {
      "Type": "AWS::ElasticLoadBalancingV2::TargetGroup",
      "Properties": {
        "Name": "cfn-extras-api",
        "Protocol": "HTTPS",
        "Port": 443,
        "VpcId": "vpc-1234",
        "TargetType": "ip"
      }
    },
    "Trust": {
      "Type": "AWS::ElasticLoadBalancingV2::TrustStore",
      "Properties": {
        "Name": "cfn-trust",
        "CaCertificatesBundleS3Bucket": "ca-bucket",
        "CaCertificatesBundleS3Key": "bundle.pem",
        "Tags": [{"Key": "Env", "Value": "prod"}]
      }
    },
    "TlsListener": {
      "Type": "AWS::ElasticLoadBalancingV2::Listener",
      "Properties": {
        "LoadBalancerArn": {"Ref": "Web"},
        "Port": 443,
        "Protocol": "HTTPS",
        "Certificates": [
          {"CertificateArn": "arn:aws:acm:us-east-1:123456789012:certificate/default-cert"}
        ],
        "DefaultActions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "ApiTg"}}
        ]
      }
    },
    "ExtraCert": {
      "Type": "AWS::ElasticLoadBalancingV2::ListenerCertificate",
      "Properties": {
        "ListenerArn": {"Ref": "TlsListener"},
        "Certificates": [
          {"CertificateArn": "arn:aws:acm:us-east-1:123456789012:certificate/sni-cert-1"}
        ]
      }
    }
  },
  "Outputs": {
    "TrustArn": {"Value": {"Ref": "Trust"}},
    "ListenerArn": {"Value": {"Ref": "TlsListener"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_elbv2_listener_certificate_and_trust_store() {
    let server = TestServer::start().await;
    let vpc = vpc_fixture(&server).await;
    let cfn = server.cloudformation_client().await;
    let elb = server.elbv2_client().await;

    cfn.create_stack()
        .stack_name("elbv2-extras-stack")
        .template_body(with_vpc_ids(EXTRAS_TEMPLATE, &vpc))
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("elbv2-extras-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().unwrap();
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let mut trust_arn = None;
    let mut listener_arn = None;
    for o in stack.outputs() {
        match o.output_key() {
            Some("TrustArn") => trust_arn = o.output_value().map(|s| s.to_string()),
            Some("ListenerArn") => listener_arn = o.output_value().map(|s| s.to_string()),
            _ => {}
        }
    }
    let trust_arn = trust_arn.expect("TrustArn");
    let listener_arn = listener_arn.expect("ListenerArn");
    assert!(trust_arn.contains(":truststore/cfn-trust/"));

    let trust_stores = elb
        .describe_trust_stores()
        .trust_store_arns(&trust_arn)
        .send()
        .await
        .expect("describe_trust_stores");
    let ts = trust_stores
        .trust_stores()
        .iter()
        .find(|t| t.trust_store_arn() == Some(&trust_arn))
        .expect("trust store present");
    assert_eq!(ts.name(), Some("cfn-trust"));
    assert_eq!(ts.status().map(|s| s.as_str()), Some("ACTIVE"));

    let listener_certs = elb
        .describe_listener_certificates()
        .listener_arn(&listener_arn)
        .send()
        .await
        .expect("describe_listener_certificates");
    let arns: Vec<&str> = listener_certs
        .certificates()
        .iter()
        .filter_map(|c| c.certificate_arn())
        .collect();
    assert!(
        arns.iter().any(|a| a.contains("sni-cert-1")),
        "SNI cert should be attached: {arns:?}"
    );

    cfn.delete_stack()
        .stack_name("elbv2-extras-stack")
        .send()
        .await
        .expect("delete_stack");

    let trust_after = elb
        .describe_trust_stores()
        .trust_store_arns(&trust_arn)
        .send()
        .await;
    assert!(
        trust_after
            .as_ref()
            .map(|r| r.trust_stores().is_empty())
            .unwrap_or(true),
        "trust store should be deleted",
    );
}

const UPDATE_TEMPLATE_V1: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Web": {
      "Type": "AWS::ElasticLoadBalancingV2::LoadBalancer",
      "Properties": {
        "Name": "cfn-update-web",
        "Scheme": "internet-facing",
        "Type": "application",
        "Subnets": ["subnet-aaa", "subnet-bbb"],
        "SecurityGroups": ["sg-old"],
        "IpAddressType": "ipv4"
      }
    },
    "Tg": {
      "Type": "AWS::ElasticLoadBalancingV2::TargetGroup",
      "Properties": {
        "Name": "cfn-update-tg",
        "Protocol": "HTTP",
        "Port": 80,
        "VpcId": "vpc-1234",
        "TargetType": "ip",
        "HealthCheckPath": "/v1",
        "HealthCheckIntervalSeconds": 30
      }
    },
    "Listener": {
      "Type": "AWS::ElasticLoadBalancingV2::Listener",
      "Properties": {
        "LoadBalancerArn": {"Ref": "Web"},
        "Port": 80,
        "Protocol": "HTTP",
        "DefaultActions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "Tg"}}
        ]
      }
    },
    "Rule": {
      "Type": "AWS::ElasticLoadBalancingV2::ListenerRule",
      "Properties": {
        "ListenerArn": {"Ref": "Listener"},
        "Priority": 5,
        "Conditions": [
          {"Field": "host-header", "Values": ["v1.example.com"]}
        ],
        "Actions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "Tg"}}
        ]
      }
    }
  },
  "Outputs": {
    "LbArn": {"Value": {"Ref": "Web"}},
    "LbDns": {"Value": {"Fn::GetAtt": ["Web", "DNSName"]}},
    "LbFullName": {"Value": {"Fn::GetAtt": ["Web", "LoadBalancerFullName"]}},
    "TgArn": {"Value": {"Ref": "Tg"}},
    "TgFullName": {"Value": {"Fn::GetAtt": ["Tg", "TargetGroupFullName"]}},
    "ListenerArn": {"Value": {"Ref": "Listener"}},
    "RuleArn": {"Value": {"Ref": "Rule"}}
  }
}"#;

const UPDATE_TEMPLATE_V2: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Web": {
      "Type": "AWS::ElasticLoadBalancingV2::LoadBalancer",
      "Properties": {
        "Name": "cfn-update-web",
        "Scheme": "internet-facing",
        "Type": "application",
        "Subnets": ["subnet-aaa", "subnet-bbb"],
        "SecurityGroups": ["sg-new-1", "sg-new-2"],
        "IpAddressType": "dualstack"
      }
    },
    "Tg": {
      "Type": "AWS::ElasticLoadBalancingV2::TargetGroup",
      "Properties": {
        "Name": "cfn-update-tg",
        "Protocol": "HTTP",
        "Port": 80,
        "VpcId": "vpc-1234",
        "TargetType": "ip",
        "HealthCheckPath": "/healthz",
        "HealthCheckIntervalSeconds": 15
      }
    },
    "Listener": {
      "Type": "AWS::ElasticLoadBalancingV2::Listener",
      "Properties": {
        "LoadBalancerArn": {"Ref": "Web"},
        "Port": 8080,
        "Protocol": "HTTP",
        "DefaultActions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "Tg"}}
        ]
      }
    },
    "Rule": {
      "Type": "AWS::ElasticLoadBalancingV2::ListenerRule",
      "Properties": {
        "ListenerArn": {"Ref": "Listener"},
        "Priority": 20,
        "Conditions": [
          {"Field": "host-header", "Values": ["v2.example.com"]}
        ],
        "Actions": [
          {"Type": "forward", "TargetGroupArn": {"Ref": "Tg"}}
        ]
      }
    }
  },
  "Outputs": {
    "LbArn": {"Value": {"Ref": "Web"}},
    "LbDns": {"Value": {"Fn::GetAtt": ["Web", "DNSName"]}},
    "LbFullName": {"Value": {"Fn::GetAtt": ["Web", "LoadBalancerFullName"]}},
    "TgArn": {"Value": {"Ref": "Tg"}},
    "TgFullName": {"Value": {"Fn::GetAtt": ["Tg", "TargetGroupFullName"]}},
    "ListenerArn": {"Value": {"Ref": "Listener"}},
    "RuleArn": {"Value": {"Ref": "Rule"}}
  }
}"#;

#[tokio::test]
async fn cfn_updates_propagate_to_elbv2_state() {
    let server = TestServer::start().await;
    let vpc = vpc_fixture(&server).await;
    let cfn = server.cloudformation_client().await;
    let elb = server.elbv2_client().await;

    cfn.create_stack()
        .stack_name("elbv2-update-stack")
        .template_body(with_vpc_ids(UPDATE_TEMPLATE_V1, &vpc))
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("elbv2-update-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().unwrap();
    assert_eq!(
        stack.stack_status().unwrap().as_str(),
        "CREATE_COMPLETE",
        "{:?}",
        stack.stack_status_reason()
    );

    let mut lb_arn = None;
    let mut listener_arn = None;
    let mut rule_arn = None;
    let mut tg_arn = None;
    let mut lb_dns = None;
    let mut lb_full = None;
    let mut tg_full = None;
    for o in stack.outputs() {
        match o.output_key() {
            Some("LbArn") => lb_arn = o.output_value().map(|s| s.to_string()),
            Some("ListenerArn") => listener_arn = o.output_value().map(|s| s.to_string()),
            Some("RuleArn") => rule_arn = o.output_value().map(|s| s.to_string()),
            Some("TgArn") => tg_arn = o.output_value().map(|s| s.to_string()),
            Some("LbDns") => lb_dns = o.output_value().map(|s| s.to_string()),
            Some("LbFullName") => lb_full = o.output_value().map(|s| s.to_string()),
            Some("TgFullName") => tg_full = o.output_value().map(|s| s.to_string()),
            _ => {}
        }
    }
    let lb_arn = lb_arn.expect("LbArn");
    let listener_arn = listener_arn.expect("ListenerArn");
    let rule_arn = rule_arn.expect("RuleArn");
    let tg_arn = tg_arn.expect("TgArn");
    let lb_dns = lb_dns.expect("LbDns");
    let lb_full = lb_full.expect("LbFullName");
    let tg_full = tg_full.expect("TgFullName");
    assert!(
        lb_dns.contains("cfn-update-web"),
        "DNSName GetAtt: {lb_dns}"
    );
    assert!(lb_full.starts_with("app/cfn-update-web/"));
    assert!(tg_full.starts_with("targetgroup/"));

    cfn.update_stack()
        .stack_name("elbv2-update-stack")
        .template_body(with_vpc_ids(UPDATE_TEMPLATE_V2, &vpc))
        .send()
        .await
        .expect("update_stack");

    let described2 = cfn
        .describe_stacks()
        .stack_name("elbv2-update-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack2 = described2.stacks().first().unwrap();
    assert_eq!(stack2.stack_status().unwrap().as_str(), "UPDATE_COMPLETE");

    // Physical ids must be preserved (in-place updates).
    let mut lb_arn2 = None;
    let mut listener_arn2 = None;
    let mut rule_arn2 = None;
    for o in stack2.outputs() {
        match o.output_key() {
            Some("LbArn") => lb_arn2 = o.output_value().map(|s| s.to_string()),
            Some("ListenerArn") => listener_arn2 = o.output_value().map(|s| s.to_string()),
            Some("RuleArn") => rule_arn2 = o.output_value().map(|s| s.to_string()),
            _ => {}
        }
    }
    assert_eq!(lb_arn2.as_deref(), Some(lb_arn.as_str()));
    assert_eq!(listener_arn2.as_deref(), Some(listener_arn.as_str()));
    assert_eq!(rule_arn2.as_deref(), Some(rule_arn.as_str()));

    // Verify the changes propagated to live ELBv2 state.
    let lbs = elb
        .describe_load_balancers()
        .load_balancer_arns(&lb_arn)
        .send()
        .await
        .expect("describe_load_balancers");
    let lb = lbs.load_balancers().first().expect("lb");
    let sgs: Vec<&str> = lb.security_groups().iter().map(|s| s.as_str()).collect();
    assert!(
        sgs.contains(&vpc_id_for(&vpc, "sg-new-1")),
        "sg updated: {sgs:?}"
    );
    assert!(
        sgs.contains(&vpc_id_for(&vpc, "sg-new-2")),
        "sg updated: {sgs:?}"
    );
    assert_eq!(lb.ip_address_type().map(|s| s.as_str()), Some("dualstack"));

    let tgs = elb
        .describe_target_groups()
        .target_group_arns(&tg_arn)
        .send()
        .await
        .expect("describe_target_groups");
    let tg = tgs.target_groups().first().expect("tg");
    assert_eq!(tg.health_check_path(), Some("/healthz"));
    assert_eq!(tg.health_check_interval_seconds(), Some(15));

    let listeners = elb
        .describe_listeners()
        .listener_arns(&listener_arn)
        .send()
        .await
        .expect("describe_listeners");
    let listener = listeners.listeners().first().expect("listener");
    assert_eq!(listener.port(), Some(8080));

    let rules = elb
        .describe_rules()
        .rule_arns(&rule_arn)
        .send()
        .await
        .expect("describe_rules");
    let rule = rules.rules().first().expect("rule");
    assert_eq!(rule.priority(), Some("20"));
    let host_values: Vec<&str> = rule
        .conditions()
        .iter()
        .flat_map(|c| c.values())
        .map(|s| s.as_str())
        .collect();
    assert!(
        host_values.contains(&"v2.example.com"),
        "rule host updated: {host_values:?}"
    );

    cfn.delete_stack()
        .stack_name("elbv2-update-stack")
        .send()
        .await
        .expect("delete_stack");

    let after = elb
        .describe_load_balancers()
        .load_balancer_arns(&lb_arn)
        .send()
        .await;
    assert!(after.is_err(), "lb gone after stack deletion");
}

/// Real default-VPC ids standing in for the templates' placeholders: the
/// default subnets, the VPC's default security group (`sg-deadbeef`) and three
/// security groups created for the update test.
async fn vpc_fixture(server: &TestServer) -> Vec<(&'static str, String)> {
    let subnets = server.default_subnet_ids().await;
    let ec2 = server.ec2_client().await;
    let mut out = vec![
        ("subnet-aaa", subnets[0].clone()),
        ("subnet-bbb", subnets[1].clone()),
        ("sg-deadbeef", server.default_security_group_id().await),
    ];
    for name in ["sg-old", "sg-new-1", "sg-new-2"] {
        let id = ec2
            .create_security_group()
            .group_name(format!("cfn-elbv2-{name}"))
            .description("cfn elbv2 test")
            .send()
            .await
            .unwrap()
            .group_id()
            .unwrap()
            .to_string();
        out.push((name, id));
    }
    out
}

/// The real id standing in for `placeholder`.
fn vpc_id_for<'a>(vpc: &'a [(&'static str, String)], placeholder: &str) -> &'a str {
    &vpc.iter().find(|(p, _)| *p == placeholder).unwrap().1
}

/// The template with its placeholder ids replaced by real ones.
fn with_vpc_ids(template: &str, vpc: &[(&'static str, String)]) -> String {
    // Longest placeholders first so `sg-new-1` is not clipped by a prefix.
    let mut pairs: Vec<_> = vpc.iter().collect();
    pairs.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    let mut out = template.to_string();
    for (p, id) in pairs {
        out = out.replace(&format!("\"{p}\""), &format!("\"{id}\""));
    }
    out
}
