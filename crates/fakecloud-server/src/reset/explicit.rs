//! The services [`ResetState`] clears through its own fields: one table row
//! per service, naming what a reset clears and which snapshots it writes, so
//! a reset arm cannot forget its persistence.

use super::{ResetState, Teardown};

/// One service [`ResetState`] clears through its own fields.
pub(super) struct ExplicitService {
    /// The names `/_fakecloud/reset/{service}` accepts for it.
    pub names: &'static [&'static str],
    /// The persist-hook keys of the state it clears (none for S3, whose
    /// buckets are deleted from the store instead).
    pub hooks: &'static [&'static str],
    /// Whether the full reset runs it. Off for a name that only clears state
    /// other rows already clear.
    pub in_full: bool,
    pub reset_all: fn(&ResetState, &mut Teardown),
    /// `None` when the service has no per-account state to reset.
    pub reset_account: Option<fn(&ResetState, &str, &mut Teardown)>,
}

pub(super) const EXPLICIT: &[ExplicitService] = &[
    ExplicitService {
        names: &["iam", "sts"],
        hooks: &["iam"],
        in_full: true,
        reset_all: |s, _| s.reset_iam(None),
        reset_account: Some(|s, account, _| s.reset_iam(Some(account))),
    },
    ExplicitService {
        names: &["sqs"],
        hooks: &["sqs"],
        in_full: true,
        reset_all: |s, _| s.sqs.write().reset(),
        reset_account: Some(|s, account, _| {
            // Every region of the account.
            if let Some(state) = s.sqs.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["sns"],
        hooks: &["sns"],
        in_full: true,
        reset_all: |s, _| {
            let mut sns = s.sns.write();
            sns.reset();
            sns.default_regional_mut().seed_default_opted_out();
        },
        reset_account: Some(|s, account, _| {
            let mut mas = s.sns.write();
            let default_account = mas.default_account_id() == account;
            if let Some(state) = mas.get_mut(account) {
                // Every region of the account.
                state.clear();
            }
            if default_account {
                mas.default_regional_mut().seed_default_opted_out();
            }
        }),
    },
    ExplicitService {
        names: &["events", "eventbridge"],
        hooks: &["eventbridge"],
        in_full: true,
        reset_all: |s, _| s.eb.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(eb) = s.eb.write().get_mut(account) {
                eb.reset();
            }
        }),
    },
    ExplicitService {
        names: &["ssm"],
        hooks: &["ssm"],
        in_full: true,
        reset_all: |s, _| s.ssm.write().reset(),
        reset_account: Some(|s, account, _| {
            // Every region of the account.
            if let Some(state) = s.ssm.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["dynamodb"],
        hooks: &["dynamodb"],
        in_full: true,
        reset_all: |s, _| s.dynamodb.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.dynamodb.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["lambda"],
        hooks: &["lambda"],
        in_full: true,
        reset_all: |s, teardown| s.reset_lambda(None, teardown),
        reset_account: Some(|s, account, teardown| s.reset_lambda(Some(account), teardown)),
    },
    ExplicitService {
        names: &["secretsmanager"],
        hooks: &["secretsmanager"],
        in_full: true,
        reset_all: |s, _| s.secretsmanager.write().reset(),
        reset_account: Some(|s, account, _| {
            // Every region of the account.
            if let Some(state) = s.secretsmanager.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["s3"],
        hooks: &[],
        in_full: true,
        reset_all: |s, teardown| s.reset_s3(None, teardown),
        reset_account: Some(|s, account, teardown| s.reset_s3(Some(account), teardown)),
    },
    ExplicitService {
        names: &["logs"],
        hooks: &["logs"],
        in_full: true,
        reset_all: |s, _| s.logs.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.logs.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["kms"],
        hooks: &["kms"],
        in_full: true,
        reset_all: |s, _| s.kms.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.kms.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["cloudformation"],
        hooks: &["cloudformation"],
        in_full: true,
        reset_all: |s, _| s.cloudformation.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.cloudformation.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["ses"],
        hooks: &["ses"],
        in_full: true,
        reset_all: |s, _| s.ses.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.ses.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["cognito"],
        hooks: &["cognito"],
        in_full: true,
        reset_all: |s, _| s.cognito.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.cognito.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["kinesis"],
        hooks: &["kinesis"],
        in_full: true,
        reset_all: |s, _| s.kinesis.write().reset(),
        reset_account: Some(|s, account, _| {
            // Every region of the account.
            if let Some(state) = s.kinesis.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["rds"],
        hooks: &["rds"],
        in_full: true,
        reset_all: |s, teardown| s.reset_rds(None, teardown),
        reset_account: Some(|s, account, teardown| s.reset_rds(Some(account), teardown)),
    },
    ExplicitService {
        names: &["elasticache"],
        hooks: &["elasticache"],
        in_full: true,
        reset_all: |s, teardown| s.reset_elasticache(None, teardown),
        reset_account: Some(|s, account, teardown| s.reset_elasticache(Some(account), teardown)),
    },
    ExplicitService {
        names: &["ec2"],
        hooks: &["ec2"],
        in_full: true,
        reset_all: |s, teardown| s.reset_ec2(None, teardown),
        reset_account: Some(|s, account, teardown| s.reset_ec2(Some(account), teardown)),
    },
    ExplicitService {
        names: &["ecr"],
        hooks: &["ecr"],
        in_full: true,
        reset_all: |s, _| s.ecr.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.ecr.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["ecs"],
        hooks: &["ecs"],
        in_full: true,
        reset_all: |s, teardown| s.reset_ecs(None, teardown),
        reset_account: Some(|s, account, teardown| s.reset_ecs(Some(account), teardown)),
    },
    ExplicitService {
        names: &["states", "stepfunctions"],
        hooks: &["stepfunctions"],
        in_full: true,
        reset_all: |s, _| s.stepfunctions.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.stepfunctions.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    ExplicitService {
        names: &["scheduler"],
        hooks: &["scheduler"],
        in_full: true,
        reset_all: |s, _| s.scheduler.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.scheduler.write().get_mut(account) {
                state.clear();
            }
        }),
    },
    // Both v1 (REST) and v2 (HTTP) share the SigV4 service identifier
    // `apigateway`; resetting the service clears both crates' state.
    ExplicitService {
        names: &["apigateway"],
        hooks: &["apigateway", "apigatewayv2"],
        in_full: false,
        reset_all: |s, _| {
            s.apigatewayv1.write().reset();
            s.apigatewayv2.write().reset();
        },
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.apigatewayv1.write().get_mut(account) {
                state.reset();
            }
            if let Some(state) = s.apigatewayv2.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["apigatewayv1", "apigatewayrest"],
        hooks: &["apigateway"],
        in_full: true,
        reset_all: |s, _| s.apigatewayv1.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.apigatewayv1.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["apigatewayv2"],
        hooks: &["apigatewayv2"],
        in_full: true,
        reset_all: |s, _| s.apigatewayv2.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.apigatewayv2.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["bedrock", "bedrock-runtime"],
        hooks: &["bedrock"],
        in_full: true,
        reset_all: |s, _| s.bedrock.write().reset(),
        reset_account: Some(|s, account, _| {
            if let Some(state) = s.bedrock.write().get_mut(account) {
                state.reset();
            }
        }),
    },
    ExplicitService {
        names: &["bedrock-agent"],
        hooks: &["bedrock-agent"],
        in_full: true,
        reset_all: |s, _| s.bedrock_agent.write().reset(),
        reset_account: Some(|s, account, _| {
            s.bedrock_agent.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["bedrock-agent-runtime"],
        hooks: &["bedrock-agent-runtime"],
        in_full: true,
        reset_all: |s, _| s.bedrock_agent_runtime.write().reset(),
        reset_account: Some(|s, account, _| {
            s.bedrock_agent_runtime.write().accounts.remove(account);
        }),
    },
    // The global services below (no region) still own their resources per
    // creating account; an account reset drops only that account's.
    ExplicitService {
        names: &["cloudfront"],
        hooks: &["cloudfront"],
        in_full: true,
        reset_all: |s, _| *s.cloudfront.write() = fakecloud_cloudfront::CloudFrontAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.cloudfront.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["route53"],
        hooks: &["route53"],
        in_full: true,
        reset_all: |s, _| *s.route53.write() = fakecloud_route53::Route53Accounts::new(),
        reset_account: Some(|s, account, _| {
            s.route53.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["acm"],
        hooks: &["acm"],
        in_full: true,
        reset_all: |s, _| *s.acm.write() = fakecloud_acm::AcmAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.acm.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["acm-pca", "acmpca"],
        hooks: &["acm-pca"],
        in_full: true,
        reset_all: |s, _| *s.acmpca.write() = fakecloud_acmpca::AcmPcaAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.acmpca.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["config"],
        hooks: &["config"],
        in_full: true,
        reset_all: |s, _| *s.config.write() = fakecloud_config::ConfigAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.config.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["route53resolver"],
        hooks: &["route53resolver"],
        in_full: true,
        reset_all: |s, _| {
            *s.route53resolver.write() = fakecloud_route53resolver::Route53ResolverAccounts::new()
        },
        reset_account: Some(|s, account, _| {
            s.route53resolver.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["firehose"],
        hooks: &["firehose"],
        in_full: true,
        reset_all: |s, _| *s.firehose.write() = fakecloud_firehose::FirehoseAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.firehose.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["glue"],
        hooks: &["glue"],
        in_full: true,
        reset_all: |s, _| *s.glue.write() = fakecloud_glue::GlueAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.glue.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["monitoring", "cloudwatch"],
        hooks: &["cloudwatch"],
        in_full: true,
        reset_all: |s, _| *s.cloudwatch.write() = fakecloud_cloudwatch::CloudWatchAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.cloudwatch.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["application-autoscaling"],
        hooks: &["application-autoscaling"],
        in_full: true,
        reset_all: |s, _| {
            *s.application_autoscaling.write() =
                fakecloud_application_autoscaling::ApplicationAutoScalingAccounts::new()
        },
        reset_account: Some(|s, account, _| {
            s.application_autoscaling.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["wafv2"],
        hooks: &["wafv2"],
        in_full: true,
        reset_all: |s, _| *s.wafv2.write() = fakecloud_wafv2::Wafv2Accounts::new(),
        reset_account: Some(|s, account, _| {
            s.wafv2.write().accounts.remove(account);
        }),
    },
    ExplicitService {
        names: &["athena"],
        hooks: &["athena"],
        in_full: true,
        reset_all: |s, _| *s.athena.write() = fakecloud_athena::AthenaAccounts::new(),
        reset_account: Some(|s, account, _| {
            s.athena.write().accounts.remove(account);
        }),
    },
    // Organizations is a cross-account registry, not per-account state: an
    // account's membership belongs to its organization's management account,
    // so there is nothing to reset for one account alone. The full reset
    // drops every organization.
    ExplicitService {
        names: &["organizations"],
        hooks: &["organizations"],
        in_full: true,
        reset_all: |s, _| s.organizations.write().clear(),
        reset_account: None,
    },
    // Quota values and overrides change what other services accept, so a
    // reset puts them back to the startup configuration too.
    ExplicitService {
        names: &["servicequotas"],
        hooks: &["servicequotas"],
        in_full: true,
        reset_all: |s, _| s.reset_servicequotas(),
        reset_account: Some(|s, account, _| {
            if let Some(data) = s.servicequotas.write().get_mut(account) {
                // The account still exists in its organization, and AWS
                // applies the quota request template once, at creation:
                // keep the marker so a reset does not apply it again.
                let template_checked = data.template_checked.take();
                *data = fakecloud_servicequotas::ServiceQuotasData {
                    template_checked,
                    ..Default::default()
                };
            }
        }),
    },
];
