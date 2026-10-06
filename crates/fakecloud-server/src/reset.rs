use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use fakecloud_aws::arn::Arn;
use fakecloud_core::multi_account::{AccountState, MultiAccountState};
use fakecloud_persistence::{S3Store, SnapshotHook};
use fakecloud_sdk::types;

// Make pub so main.rs can construct it
#[derive(Clone)]
pub(crate) struct ResetState {
    pub iam: fakecloud_iam::SharedIamState,
    pub sqs: fakecloud_sqs::SharedSqsState,
    pub sns: fakecloud_sns::SharedSnsState,
    pub eb: fakecloud_eventbridge::SharedEventBridgeState,
    pub ssm: fakecloud_ssm::SharedSsmState,
    pub dynamodb: fakecloud_dynamodb::SharedDynamoDbState,
    pub lambda: fakecloud_lambda::SharedLambdaState,
    pub secretsmanager: fakecloud_secretsmanager::SharedSecretsManagerState,
    pub s3: fakecloud_s3::SharedS3State,
    pub logs: fakecloud_logs::SharedLogsState,
    pub kms: fakecloud_kms::SharedKmsState,
    pub cloudformation: fakecloud_cloudformation::SharedCloudFormationState,
    pub ses: fakecloud_ses::SharedSesState,
    pub cognito: fakecloud_cognito::SharedCognitoState,
    pub kinesis: fakecloud_kinesis::SharedKinesisState,
    pub rds: fakecloud_rds::SharedRdsState,
    pub elasticache: fakecloud_elasticache::SharedElastiCacheState,
    pub ecr: fakecloud_ecr::SharedEcrState,
    pub ecs: fakecloud_ecs::SharedEcsState,
    pub stepfunctions: fakecloud_stepfunctions::SharedStepFunctionsState,
    pub scheduler: fakecloud_scheduler::SharedSchedulerState,
    pub apigatewayv1: fakecloud_apigateway::SharedApiGatewayState,
    pub apigatewayv2: fakecloud_apigatewayv2::SharedApiGatewayV2State,
    pub bedrock: fakecloud_bedrock::SharedBedrockState,
    pub bedrock_agent: fakecloud_bedrock_agent::SharedBedrockAgentState,
    pub bedrock_agent_runtime: fakecloud_bedrock_agent_runtime::SharedBedrockAgentRuntimeState,
    pub cloudfront: fakecloud_cloudfront::SharedCloudFrontState,
    pub route53: fakecloud_route53::SharedRoute53State,
    pub acm: fakecloud_acm::SharedAcmState,
    pub acmpca: fakecloud_acmpca::SharedAcmPcaState,
    pub config: fakecloud_config::SharedConfigState,
    pub route53resolver: fakecloud_route53resolver::SharedRoute53ResolverState,
    pub firehose: fakecloud_firehose::SharedFirehoseState,
    pub glue: fakecloud_glue::SharedGlueState,
    pub cloudwatch: fakecloud_cloudwatch::SharedCloudWatchState,
    pub application_autoscaling:
        fakecloud_application_autoscaling::SharedApplicationAutoScalingState,
    pub wafv2: fakecloud_wafv2::SharedWafv2State,
    pub athena: fakecloud_athena::SharedAthenaState,
    pub organizations: fakecloud_organizations::SharedOrganizationsState,
    pub servicequotas: fakecloud_servicequotas::SharedServiceQuotasState,
    pub servicequotas_settings: fakecloud_servicequotas::SharedQuotaSettings,
    /// The quota settings the server started with, which a reset restores.
    pub servicequotas_baseline: fakecloud_servicequotas::QuotaSettings,
    pub container_runtime: Option<Arc<fakecloud_lambda::runtime::ContainerRuntime>>,
    pub rds_runtime: Option<Arc<fakecloud_rds::runtime::RdsRuntime>>,
    pub elasticache_runtime: Option<Arc<fakecloud_elasticache::runtime::ElastiCacheRuntime>>,
    pub ecs_runtime: Option<Arc<fakecloud_ecs::runtime::EcsRuntime>>,
    pub ec2: fakecloud_ec2::SharedEc2State,
    pub ec2_runtime: Option<Arc<fakecloud_ec2::runtime::Ec2Runtime>>,
    /// Wiring that only exists once every service is built (the snapshot
    /// hooks, the S3 disk store, the services reset through a registered
    /// entry). Filled once by `main.rs` before the server starts serving.
    pub late: Arc<OnceLock<LateReset>>,
}

/// A snapshot-backed service the reset endpoints clear through a registered
/// entry rather than a dedicated [`ResetState`] field.
pub(crate) struct ServiceReset {
    /// The names `/_fakecloud/reset/{service}` accepts for it.
    names: &'static [&'static str],
    /// Its key among [`LateReset::hooks`].
    hook: &'static str,
    reset_all: Box<ResetAllFn>,
    reset_account: Box<ResetAccountFn>,
}

/// Clears a service in every account, queuing any teardown it needs.
type ResetAllFn = dyn Fn(&mut Teardown) + Send + Sync;
/// Clears one account of a service, queuing any teardown it needs.
type ResetAccountFn = dyn Fn(&str, &mut Teardown) + Send + Sync;

impl ServiceReset {
    pub(crate) fn new(
        names: &'static [&'static str],
        hook: &'static str,
        reset_all: impl Fn(&mut Teardown) + Send + Sync + 'static,
        reset_account: impl Fn(&str, &mut Teardown) + Send + Sync + 'static,
    ) -> Self {
        Self {
            names,
            hook,
            reset_all: Box::new(reset_all),
            reset_account: Box::new(reset_account),
        }
    }

    /// A service whose whole state is one [`MultiAccountState`].
    pub(crate) fn multi_account<T>(
        names: &'static [&'static str],
        hook: &'static str,
        state: Arc<parking_lot::RwLock<MultiAccountState<T>>>,
    ) -> Self
    where
        T: AccountState + Send + Sync + 'static,
    {
        let all = state.clone();
        Self::new(
            names,
            hook,
            move |_| all.write().reset(),
            move |account_id, _| state.write().reset_account(account_id),
        )
    }
}

/// See [`ResetState::late`].
#[derive(Default)]
pub(crate) struct LateReset {
    /// Every snapshot-backed service's persist hook, keyed like the
    /// CloudFormation provisioner's (`cfn_snapshot_hooks`). Empty in memory
    /// mode.
    pub hooks: BTreeMap<&'static str, SnapshotHook>,
    pub services: Vec<ServiceReset>,
    /// The S3 store, whose buckets live on disk one directory each rather than
    /// in a snapshot.
    pub s3_store: Option<Arc<dyn S3Store>>,
}

impl LateReset {
    /// The persist-hook keys no reset clears: each one is a service whose
    /// state survives `/_fakecloud/reset` on disk. Empty when every
    /// snapshot-backed service is wired into the reset.
    pub(crate) fn hooks_without_reset(&self) -> Vec<&'static str> {
        self.hooks
            .keys()
            .copied()
            .filter(|key| {
                !EXPLICIT_HOOKS.contains(key) && !self.services.iter().any(|s| s.hook == *key)
            })
            .collect()
    }

    /// The persist-hook keys a reset would run but no service registered:
    /// a reset of that service would not reach the disk.
    pub(crate) fn missing_hooks(&self) -> Vec<&'static str> {
        EXPLICIT_HOOKS
            .iter()
            .copied()
            .chain(self.services.iter().map(|s| s.hook))
            .filter(|key| !self.hooks.contains_key(key))
            .collect()
    }
}

/// The persist-hook keys of the services [`ResetState`] clears through its
/// own fields (S3 has none: its buckets are deleted from the store).
const EXPLICIT_HOOKS: &[&str] = &[
    "iam",
    "sqs",
    "sns",
    "eventbridge",
    "ssm",
    "dynamodb",
    "lambda",
    "secretsmanager",
    "logs",
    "kms",
    "cloudformation",
    "ses",
    "cognito",
    "kinesis",
    "rds",
    "elasticache",
    "ec2",
    "ecr",
    "ecs",
    "stepfunctions",
    "scheduler",
    "apigateway",
    "apigatewayv2",
    "bedrock",
    "bedrock-agent",
    "bedrock-agent-runtime",
    "cloudfront",
    "route53",
    "acm",
    "acm-pca",
    "config",
    "route53resolver",
    "firehose",
    "glue",
    "cloudwatch",
    "application-autoscaling",
    "wafv2",
    "athena",
    "organizations",
    "servicequotas",
];

// A reset snapshots the reset rows' incarnation ids and volumes and clears
// the state under one write lock, then tears down by those ids. Runtime
// records are keyed by incarnation, so a resource created after the reset
// (even under a reset one's identifier) is never reached, and a start still in
// flight for a reset incarnation reaps itself once it finds its row gone.

/// `(DbiResourceId, data volume)` of every RDS instance in an account.
fn rds_incarnations(state: &fakecloud_rds::RdsState) -> Vec<(String, String)> {
    let tag = fakecloud_core::data_volume::current_scope().tag();
    state
        .instances
        .values()
        .map(|inst| {
            (
                inst.dbi_resource_id.clone(),
                inst.data_volume_name(tag, &state.account_id),
            )
        })
        .collect()
}

/// `(account, instance id)` of every EC2 instance in an account, whose
/// containers and data volumes a reset removes (stopped ones included).
fn ec2_instances(state: &fakecloud_ec2::Ec2State) -> Vec<(String, String)> {
    state
        .instances
        .keys()
        .map(|id| (state.account_id.clone(), id.clone()))
        .collect()
}

/// `(incarnation, data volume)` of every cache cluster, replication group and
/// serverless cache in an account (no volume for memcached).
fn elasticache_incarnations(
    state: &fakecloud_elasticache::ElastiCacheState,
) -> Vec<(String, Option<String>)> {
    let tag = fakecloud_core::data_volume::current_scope().tag();
    let account = &state.account_id;
    let clusters = state.cache_clusters.values().map(|c| {
        (
            c.incarnation(),
            (c.engine != "memcached").then(|| c.data_volume_name(tag, account)),
        )
    });
    let groups = state.replication_groups.values().map(|g| {
        (
            g.incarnation(),
            (g.engine != "memcached").then(|| g.data_volume_name(tag, account)),
        )
    });
    let serverless = state
        .serverless_caches
        .values()
        .map(|c| (c.incarnation(), Some(c.data_volume_name(tag, account))));
    clusters.chain(groups).chain(serverless).collect()
}

/// How long a reset response waits for its container teardown.
const TEARDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Container and data-volume teardown a reset queued. The reset handlers
/// await it before replying, so once a reset returns, a resource recreated
/// under a reset one's identifier can't race the teardown and mount the old
/// data volume (or have its new container stopped).
///
/// It also carries the writes that put the reset on disk in persistent mode
/// (each reset service's snapshot, the S3 bucket directories), so a restart
/// does not bring the reset state back.
#[derive(Default)]
pub(crate) struct Teardown {
    tasks: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
    /// The persist hooks queued, by key.
    persisted: Vec<&'static str>,
}

impl Teardown {
    pub(crate) fn push(&mut self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.push(Box::pin(f));
    }

    /// Run the teardown on its own task (a client that hangs up mid-reset
    /// can't cancel it half way, leaving containers untracked and volumes
    /// behind) and wait for it, bounded so a wedged daemon can't hang the
    /// reset response; past the bound it keeps running in the background.
    pub(crate) async fn run(self) {
        // Each service's teardown runs on its own task, concurrently, so a
        // slow one can't eat the others' share of the wait.
        let tasks: Vec<_> = self.tasks.into_iter().map(tokio::spawn).collect();
        let deadline = tokio::time::Instant::now() + TEARDOWN_WAIT;
        for task in tasks {
            match tokio::time::timeout_at(deadline, task).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::error!(%err, "reset teardown failed; containers or volumes may be left behind");
                }
                Err(_) => {
                    tracing::warn!(
                        "reset teardown still running after {}s; continuing in the background",
                        TEARDOWN_WAIT.as_secs()
                    );
                }
            }
        }
    }
}

impl ResetState {
    /// Reset RDS in every account, stopping the backing containers and
    /// dropping the instances' data volumes: the instances are gone for good,
    /// so one recreated under the same identifier must start clean (the
    /// volumes would otherwise outlive the state, #2630).
    fn reset_rds(&self, teardown: &mut Teardown) {
        let gone: Vec<(String, String)> = {
            let mut mas = self.rds.write();
            let gone = mas.iter().flat_map(|(_, s)| rds_incarnations(s)).collect();
            mas.reset();
            gone
        };
        if let Some(rt) = self.rds_runtime.clone() {
            teardown.push(async move {
                for (incarnation, volume) in gone {
                    rt.stop(&incarnation).await;
                    rt.remove_data_volume_named(&volume).await;
                }
            });
        }
    }

    /// Reset ElastiCache in every account, stopping the backing containers
    /// and dropping the resources' data volumes (see [`Self::reset_rds`]).
    fn reset_elasticache(&self, teardown: &mut Teardown) {
        let gone: Vec<(String, Option<String>)> = {
            let mut mas = self.elasticache.write();
            let gone = mas
                .iter()
                .flat_map(|(_, s)| elasticache_incarnations(s))
                .collect();
            mas.reset();
            gone
        };
        if let Some(rt) = self.elasticache_runtime.clone() {
            teardown.push(async move {
                for (incarnation, volume) in gone {
                    rt.stop(&incarnation).await;
                    if let Some(volume) = volume {
                        rt.remove_data_volume_named(&volume).await;
                    }
                }
            });
        }
    }

    /// Reset EC2 in every account, tearing down every instance's container
    /// and data volume by instance id (see [`Self::reset_rds`]).
    fn reset_ec2(&self, teardown: &mut Teardown) {
        let gone: Vec<(String, String)> = {
            let mut mas = self.ec2.write();
            let gone = mas.iter().flat_map(|(_, s)| ec2_instances(s)).collect();
            mas.reset();
            gone
        };
        if let Some(rt) = self.ec2_runtime.clone() {
            teardown.push(async move { rt.remove_instances(gone).await });
        }
    }

    /// Reset Service Quotas: every account's applied values, requests and
    /// overrides, and the enforcement settings back to the startup flags.
    fn reset_servicequotas(&self) {
        self.servicequotas.write().reset();
        *self.servicequotas_settings.write() = self.servicequotas_baseline.clone();
    }

    /// The late wiring, or an empty one before `main.rs` filled it (unit
    /// tests, and a reset racing startup, which has nothing on disk to reach).
    fn late(&self) -> &LateReset {
        static EMPTY: OnceLock<LateReset> = OnceLock::new();
        self.late
            .get()
            .unwrap_or_else(|| EMPTY.get_or_init(LateReset::default))
    }

    /// The registered service `service` names, if any.
    fn registered(&self, service: &str) -> Option<&ServiceReset> {
        self.late()
            .services
            .iter()
            .find(|s| s.names.contains(&service))
    }

    /// Queue the persist hooks of `keys` on `teardown`, so the reset state is
    /// written through to disk before the reset replies. Memory mode
    /// registers no hooks, so this queues nothing there.
    fn persist(&self, teardown: &mut Teardown, keys: &[&'static str]) {
        let hooks = &self.late().hooks;
        for key in keys {
            teardown.persisted.push(key);
            if let Some(hook) = hooks.get(key) {
                teardown.push(hook());
            }
        }
    }

    /// Delete `buckets` from the S3 store: each one is a directory on disk
    /// in persistent mode, which the next start would load back.
    fn delete_s3_buckets(&self, teardown: &mut Teardown, buckets: Vec<String>) {
        let Some(store) = self.late().s3_store.clone() else {
            return;
        };
        if buckets.is_empty() {
            return;
        }
        teardown.push(async move {
            let deleted = tokio::task::spawn_blocking(move || {
                for bucket in buckets {
                    if let Err(err) = store.delete_bucket(&bucket) {
                        tracing::error!(%bucket, %err, "reset could not delete the bucket from disk");
                    }
                }
            })
            .await;
            if let Err(err) = deleted {
                tracing::error!(%err, "reset S3 bucket deletion task panicked");
            }
        });
    }

    /// Reset S3 in every account, deleting the buckets from the store too.
    fn reset_s3(&self, teardown: &mut Teardown) {
        let buckets: Vec<String> = {
            let mut mas = self.s3.write();
            let buckets = mas
                .iter()
                .flat_map(|(_, s)| s.buckets.keys().cloned())
                .collect();
            mas.reset();
            buckets
        };
        self.delete_s3_buckets(teardown, buckets);
    }

    pub(crate) fn reset_service(&self, service: &str) -> Result<Teardown, String> {
        let mut teardown = Teardown::default();
        let keys: &[&'static str] = match service {
            "servicequotas" => {
                self.reset_servicequotas();
                &["servicequotas"]
            }
            "iam" | "sts" => {
                // The reset drops the execution-role sessions warm Lambda
                // instances hold: stop handing them invocations in the same
                // step, then tear the free ones down in the background.
                if let Some(ref rt) = self.container_runtime {
                    rt.mark_credentials_revoked(None);
                }
                self.iam.write().reset();
                if let Some(ref rt) = self.container_runtime {
                    let rt = rt.clone();
                    tokio::spawn(async move { rt.retire_released().await });
                }
                &["iam"]
            }
            "sqs" => {
                self.sqs.write().reset();
                &["sqs"]
            }
            "sns" => {
                let mut s = self.sns.write();
                s.reset();
                s.default_regional_mut().seed_default_opted_out();
                &["sns"]
            }
            "events" | "eventbridge" => {
                self.eb.write().reset();
                &["eventbridge"]
            }
            "ssm" => {
                self.ssm.write().reset();
                &["ssm"]
            }
            "dynamodb" => {
                self.dynamodb.write().reset();
                &["dynamodb"]
            }
            "lambda" => {
                self.lambda.write().reset();
                if let Some(ref rt) = self.container_runtime {
                    let rt = rt.clone();
                    tokio::spawn(async move { rt.stop_all().await });
                }
                &["lambda"]
            }
            "secretsmanager" => {
                self.secretsmanager.write().reset();
                &["secretsmanager"]
            }
            "s3" => {
                self.reset_s3(&mut teardown);
                &[]
            }
            "logs" => {
                self.logs.write().reset();
                &["logs"]
            }
            "kms" => {
                self.kms.write().reset();
                &["kms"]
            }
            "cloudformation" => {
                self.cloudformation.write().reset();
                &["cloudformation"]
            }
            "ses" => {
                self.ses.write().reset();
                &["ses"]
            }
            "cognito" => {
                self.cognito.write().reset();
                &["cognito"]
            }
            "kinesis" => {
                self.kinesis.write().reset();
                &["kinesis"]
            }
            "rds" => {
                self.reset_rds(&mut teardown);
                &["rds"]
            }
            "elasticache" => {
                self.reset_elasticache(&mut teardown);
                &["elasticache"]
            }
            "ec2" => {
                self.reset_ec2(&mut teardown);
                &["ec2"]
            }
            "ecr" => {
                self.ecr.write().reset();
                &["ecr"]
            }
            "ecs" => {
                self.ecs.write().reset();
                if let Some(ref rt) = self.ecs_runtime {
                    let rt = rt.clone();
                    tokio::spawn(async move { rt.stop_all().await });
                }
                &["ecs"]
            }
            "states" | "stepfunctions" => {
                self.stepfunctions.write().reset();
                &["stepfunctions"]
            }
            "scheduler" => {
                self.scheduler.write().reset();
                &["scheduler"]
            }
            "apigateway" => {
                // Both v1 (REST) and v2 (HTTP) share the SigV4 service
                // identifier `apigateway`; resetting the service clears
                // both crates' state.
                self.apigatewayv1.write().reset();
                self.apigatewayv2.write().reset();
                &["apigateway", "apigatewayv2"]
            }
            "apigatewayv1" | "apigatewayrest" => {
                self.apigatewayv1.write().reset();
                &["apigateway"]
            }
            "apigatewayv2" => {
                self.apigatewayv2.write().reset();
                &["apigatewayv2"]
            }
            "bedrock" | "bedrock-runtime" => {
                self.bedrock.write().reset();
                &["bedrock"]
            }
            "bedrock-agent" => {
                self.bedrock_agent.write().reset();
                &["bedrock-agent"]
            }
            "bedrock-agent-runtime" => {
                self.bedrock_agent_runtime.write().reset();
                &["bedrock-agent-runtime"]
            }
            "cloudfront" => {
                *self.cloudfront.write() = fakecloud_cloudfront::CloudFrontAccounts::new();
                &["cloudfront"]
            }
            "route53" => {
                *self.route53.write() = fakecloud_route53::Route53Accounts::new();
                &["route53"]
            }
            "acm" => {
                *self.acm.write() = fakecloud_acm::AcmAccounts::new();
                &["acm"]
            }
            "acm-pca" | "acmpca" => {
                *self.acmpca.write() = fakecloud_acmpca::AcmPcaAccounts::new();
                &["acm-pca"]
            }
            "config" => {
                *self.config.write() = fakecloud_config::ConfigAccounts::new();
                &["config"]
            }
            "route53resolver" => {
                *self.route53resolver.write() =
                    fakecloud_route53resolver::Route53ResolverAccounts::new();
                &["route53resolver"]
            }
            "firehose" => {
                *self.firehose.write() = fakecloud_firehose::FirehoseAccounts::new();
                &["firehose"]
            }
            "glue" => {
                *self.glue.write() = fakecloud_glue::GlueAccounts::new();
                &["glue"]
            }
            "monitoring" | "cloudwatch" => {
                *self.cloudwatch.write() = fakecloud_cloudwatch::CloudWatchAccounts::new();
                &["cloudwatch"]
            }
            "application-autoscaling" => {
                *self.application_autoscaling.write() =
                    fakecloud_application_autoscaling::ApplicationAutoScalingAccounts::new();
                &["application-autoscaling"]
            }
            "wafv2" => {
                *self.wafv2.write() = fakecloud_wafv2::Wafv2Accounts::new();
                &["wafv2"]
            }
            "athena" => {
                *self.athena.write() = fakecloud_athena::AthenaAccounts::new();
                &["athena"]
            }
            "organizations" => {
                self.organizations.write().clear();
                &["organizations"]
            }
            _ => match self.registered(service) {
                Some(entry) => {
                    (entry.reset_all)(&mut teardown);
                    std::slice::from_ref(&entry.hook)
                }
                None => return Err(format!("Unknown service: {service}")),
            },
        };
        self.persist(&mut teardown, keys);
        tracing::info!(service = %service, "service state reset via per-service reset API");
        Ok(teardown)
    }

    /// Reset a single service's state for a specific account only.
    pub(crate) fn reset_service_for_account(
        &self,
        service: &str,
        account_id: &str,
    ) -> Result<Teardown, String> {
        let mut teardown = Teardown::default();
        let keys: &[&'static str] = match service {
            "servicequotas" => {
                if let Some(data) = self.servicequotas.write().get_mut(account_id) {
                    // The account still exists in its organization, and AWS
                    // applies the quota request template once, at creation:
                    // keep the marker so a reset does not apply it again.
                    let template_checked = data.template_checked.take();
                    *data = fakecloud_servicequotas::ServiceQuotasData {
                        template_checked,
                        ..Default::default()
                    };
                }
                &["servicequotas"]
            }
            "iam" | "sts" => {
                if let Some(ref rt) = self.container_runtime {
                    rt.mark_credentials_revoked(Some(account_id));
                }
                {
                    let mut mas = self.iam.write();
                    let region = mas.region().to_string();
                    if let Some(state) = mas.get_mut(account_id) {
                        state.reset(&region);
                    }
                }
                if let Some(ref rt) = self.container_runtime {
                    let rt = rt.clone();
                    tokio::spawn(async move { rt.retire_released().await });
                }
                &["iam"]
            }
            "sqs" => {
                // Every region of the account.
                let mut mas = self.sqs.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["sqs"]
            }
            "sns" => {
                let mut mas = self.sns.write();
                let default_account = mas.default_account_id() == account_id;
                if let Some(state) = mas.get_mut(account_id) {
                    // Every region of the account.
                    state.clear();
                }
                if default_account {
                    mas.default_regional_mut().seed_default_opted_out();
                }
                &["sns"]
            }
            "events" | "eventbridge" => {
                let mut mas = self.eb.write();
                if let Some(eb) = mas.get_mut(account_id) {
                    eb.reset();
                }
                &["eventbridge"]
            }
            "ssm" => {
                // Every region of the account.
                let mut mas = self.ssm.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["ssm"]
            }
            "dynamodb" => {
                let mut mas = self.dynamodb.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["dynamodb"]
            }
            "lambda" => {
                let mut mas = self.lambda.write();
                if let Some(state) = mas.get_mut(account_id) {
                    // Every region of the account.
                    state.clear();
                }
                &["lambda"]
            }
            "secretsmanager" => {
                // Every region of the account.
                let mut mas = self.secretsmanager.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["secretsmanager"]
            }
            "s3" => {
                let buckets: Vec<String> = {
                    let mut mas = self.s3.write();
                    match mas.get_mut(account_id) {
                        Some(state) => {
                            let buckets = state.buckets.keys().cloned().collect();
                            state.reset();
                            buckets
                        }
                        None => Vec::new(),
                    }
                };
                self.delete_s3_buckets(&mut teardown, buckets);
                &[]
            }
            "logs" => {
                let mut mas = self.logs.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["logs"]
            }
            "kms" => {
                let mut mas = self.kms.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["kms"]
            }
            "cloudformation" => {
                let mut mas = self.cloudformation.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["cloudformation"]
            }
            "ses" => {
                let mut mas = self.ses.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["ses"]
            }
            "cognito" => {
                let mut mas = self.cognito.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["cognito"]
            }
            "kinesis" => {
                // Every region of the account.
                let mut mas = self.kinesis.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["kinesis"]
            }
            "rds" => {
                let mut mas = self.rds.write();
                if let Some(state) = mas.get_mut(account_id) {
                    let gone = rds_incarnations(state);
                    state.reset();
                    // The account's instances are gone: stop their containers
                    // and drop their data volumes by incarnation, so an
                    // instance recreated under the same identifier (or another
                    // account's same-named one) is never reached.
                    if let Some(rt) = self.rds_runtime.clone() {
                        teardown.push(async move {
                            for (incarnation, volume) in gone {
                                rt.stop(&incarnation).await;
                                rt.remove_data_volume_named(&volume).await;
                            }
                        });
                    }
                }
                &["rds"]
            }
            "elasticache" => {
                let mut mas = self.elasticache.write();
                if let Some(state) = mas.get_mut(account_id) {
                    let gone = elasticache_incarnations(state);
                    state.reset();
                    if let Some(rt) = self.elasticache_runtime.clone() {
                        teardown.push(async move {
                            for (incarnation, volume) in gone {
                                rt.stop(&incarnation).await;
                                if let Some(volume) = volume {
                                    rt.remove_data_volume_named(&volume).await;
                                }
                            }
                        });
                    }
                }
                &["elasticache"]
            }
            "ecr" => {
                let mut mas = self.ecr.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["ecr"]
            }
            "ecs" => {
                let mut mas = self.ecs.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["ecs"]
            }
            "states" | "stepfunctions" => {
                let mut mas = self.stepfunctions.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["stepfunctions"]
            }
            "scheduler" => {
                let mut mas = self.scheduler.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.clear();
                }
                &["scheduler"]
            }
            "apigateway" => {
                let mut v1 = self.apigatewayv1.write();
                if let Some(state) = v1.get_mut(account_id) {
                    state.reset();
                }
                let mut v2 = self.apigatewayv2.write();
                if let Some(state) = v2.get_mut(account_id) {
                    state.reset();
                }
                &["apigateway", "apigatewayv2"]
            }
            "apigatewayv1" | "apigatewayrest" => {
                let mut mas = self.apigatewayv1.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["apigateway"]
            }
            "apigatewayv2" => {
                let mut mas = self.apigatewayv2.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["apigatewayv2"]
            }
            "bedrock" | "bedrock-runtime" => {
                let mut mas = self.bedrock.write();
                if let Some(state) = mas.get_mut(account_id) {
                    state.reset();
                }
                &["bedrock"]
            }
            "bedrock-agent" => {
                self.bedrock_agent.write().accounts.remove(account_id);
                &["bedrock-agent"]
            }
            "bedrock-agent-runtime" => {
                self.bedrock_agent_runtime
                    .write()
                    .accounts
                    .remove(account_id);
                &["bedrock-agent-runtime"]
            }
            "cloudfront" => {
                // CloudFront is global (no region) but its resources are
                // owned by the creating account; drop only that account's.
                self.cloudfront.write().accounts.remove(account_id);
                &["cloudfront"]
            }
            "route53" => {
                // Route 53 is global (no region) but its resources are owned
                // by the creating account; drop only that account's.
                self.route53.write().accounts.remove(account_id);
                &["route53"]
            }
            "acm" => {
                self.acm.write().accounts.remove(account_id);
                &["acm"]
            }
            "acm-pca" | "acmpca" => {
                self.acmpca.write().accounts.remove(account_id);
                &["acm-pca"]
            }
            "config" => {
                self.config.write().accounts.remove(account_id);
                &["config"]
            }
            "route53resolver" => {
                self.route53resolver.write().accounts.remove(account_id);
                &["route53resolver"]
            }
            "firehose" => {
                self.firehose.write().accounts.remove(account_id);
                &["firehose"]
            }
            "glue" => {
                self.glue.write().accounts.remove(account_id);
                &["glue"]
            }
            "monitoring" | "cloudwatch" => {
                self.cloudwatch.write().accounts.remove(account_id);
                &["cloudwatch"]
            }
            "application-autoscaling" => {
                self.application_autoscaling
                    .write()
                    .accounts
                    .remove(account_id);
                &["application-autoscaling"]
            }
            "wafv2" => {
                self.wafv2.write().accounts.remove(account_id);
                &["wafv2"]
            }
            "athena" => {
                self.athena.write().accounts.remove(account_id);
                &["athena"]
            }
            _ => match self.registered(service) {
                Some(entry) => {
                    (entry.reset_account)(account_id, &mut teardown);
                    std::slice::from_ref(&entry.hook)
                }
                None => return Err(format!("Unknown service: {service}")),
            },
        };
        self.persist(&mut teardown, keys);
        tracing::info!(service = %service, account_id = %account_id, "service state reset for account via per-account reset API");
        Ok(teardown)
    }

    pub(crate) fn reset(&self) -> (axum::Json<types::ResetResponse>, Teardown) {
        let mut teardown = Teardown::default();
        self.iam.write().reset();
        self.sqs.write().reset();
        {
            let mut sns = self.sns.write();
            sns.reset();
            sns.default_regional_mut().seed_default_opted_out();
        }
        self.eb.write().reset();
        self.ssm.write().reset();
        self.dynamodb.write().reset();
        self.lambda.write().reset();
        // Stop all Lambda containers on reset
        if let Some(ref rt) = self.container_runtime {
            let rt = rt.clone();
            tokio::spawn(async move { rt.stop_all().await });
        }
        self.secretsmanager.write().reset();
        self.reset_s3(&mut teardown);
        self.logs.write().reset();
        self.kms.write().reset();
        self.cloudformation.write().reset();
        self.ses.write().reset();
        self.cognito.write().reset();
        self.kinesis.write().reset();
        self.reset_rds(&mut teardown);
        self.reset_elasticache(&mut teardown);
        self.reset_ec2(&mut teardown);
        self.ecr.write().reset();
        self.ecs.write().reset();
        if let Some(ref rt) = self.ecs_runtime {
            let rt = rt.clone();
            tokio::spawn(async move { rt.stop_all().await });
        }
        self.stepfunctions.write().reset();
        self.scheduler.write().reset();
        self.apigatewayv1.write().reset();
        self.apigatewayv2.write().reset();
        self.bedrock.write().reset();
        self.bedrock_agent.write().reset();
        self.bedrock_agent_runtime.write().reset();
        *self.cloudfront.write() = fakecloud_cloudfront::CloudFrontAccounts::new();
        *self.route53.write() = fakecloud_route53::Route53Accounts::new();
        *self.acm.write() = fakecloud_acm::AcmAccounts::new();
        *self.acmpca.write() = fakecloud_acmpca::AcmPcaAccounts::new();
        *self.config.write() = fakecloud_config::ConfigAccounts::new();
        *self.route53resolver.write() = fakecloud_route53resolver::Route53ResolverAccounts::new();
        *self.firehose.write() = fakecloud_firehose::FirehoseAccounts::new();
        *self.glue.write() = fakecloud_glue::GlueAccounts::new();
        *self.cloudwatch.write() = fakecloud_cloudwatch::CloudWatchAccounts::new();
        *self.application_autoscaling.write() =
            fakecloud_application_autoscaling::ApplicationAutoScalingAccounts::new();
        *self.wafv2.write() = fakecloud_wafv2::Wafv2Accounts::new();
        *self.athena.write() = fakecloud_athena::AthenaAccounts::new();
        // Organizations is a cross-account registry (not MultiAccountState);
        // a full reset drops every organization so subsequent runs start
        // with none, matching the no-in-use default state.
        self.organizations.write().clear();
        // Quota values and overrides change what other services accept, so a
        // reset puts them back to the startup configuration too.
        self.reset_servicequotas();
        let late = self.late();
        for entry in &late.services {
            (entry.reset_all)(&mut teardown);
        }
        // Every snapshot-backed service was just reset: write each one's
        // (now empty) state through to disk.
        let keys: Vec<&'static str> = EXPLICIT_HOOKS
            .iter()
            .copied()
            .chain(late.services.iter().map(|s| s.hook))
            .collect();
        self.persist(&mut teardown, &keys);
        tracing::info!("state reset via reset API");
        (
            axum::Json(types::ResetResponse {
                status: "ok".to_string(),
            }),
            teardown,
        )
    }
}

/// The shared state of every snapshot-backed service without a dedicated
/// [`ResetState`] field, plus the runtimes their reset tears down.
pub(crate) struct ResetServiceStates {
    pub account: fakecloud_account::SharedAccountState,
    pub amplify: fakecloud_amplify::SharedAmplifyState,
    pub appconfig: fakecloud_appconfig::SharedAppConfigState,
    pub appsync: fakecloud_appsync::SharedAppSyncState,
    pub autoscaling: fakecloud_autoscaling::SharedAutoScalingState,
    pub backup: fakecloud_backup::SharedBackupState,
    pub batch: fakecloud_batch::SharedBatchState,
    pub ce: fakecloud_ce::SharedCeState,
    pub cloudcontrol: fakecloud_cloudcontrol::SharedCloudControlState,
    pub cloudtrail: fakecloud_cloudtrail::SharedCloudTrailState,
    pub codeartifact: fakecloud_codeartifact::SharedCodeArtifactState,
    pub codebuild: fakecloud_codebuild::SharedCodeBuildState,
    pub codecommit: fakecloud_codecommit::SharedCodeCommitState,
    pub codeconnections: fakecloud_codeconnections::SharedCodeConnectionsState,
    pub codedeploy: fakecloud_codedeploy::SharedCodeDeployState,
    pub codepipeline: fakecloud_codepipeline::SharedCodePipelineState,
    pub comprehend: fakecloud_comprehend::SharedComprehendState,
    pub dms: fakecloud_dms::SharedDmsState,
    pub docdb: fakecloud_docdb::SharedDocDbState,
    pub dsql: fakecloud_dsql::SharedDsqlState,
    pub efs: fakecloud_efs::SharedEfsState,
    pub eks: fakecloud_eks::SharedEksState,
    pub elasticbeanstalk: fakecloud_elasticbeanstalk::SharedEbState,
    pub elbv2: fakecloud_elbv2::SharedElbv2State,
    pub emr: fakecloud_emr::SharedEmrState,
    pub fis: fakecloud_fis::SharedFisState,
    pub glacier: fakecloud_glacier::SharedGlacierState,
    pub identitystore: fakecloud_identitystore::SharedIdentityStoreState,
    pub iot: fakecloud_iot::SharedIotState,
    pub iotdata: fakecloud_iotdata::SharedIotDataState,
    pub iotwireless: fakecloud_iotwireless::SharedIotWirelessState,
    pub kafka: fakecloud_kafka::SharedKafkaState,
    pub kinesisanalyticsv2: fakecloud_kinesisanalyticsv2::SharedKa2State,
    pub lakeformation: fakecloud_lakeformation::SharedLakeFormationState,
    pub managedblockchain: fakecloud_managedblockchain::SharedManagedBlockchainState,
    pub mediaconvert: fakecloud_mediaconvert::SharedMediaConvertState,
    pub memorydb: fakecloud_memorydb::SharedMemoryDbState,
    pub mq: fakecloud_mq::SharedMqState,
    pub mwaa: fakecloud_mwaa::SharedMwaaState,
    pub neptune: fakecloud_neptune::SharedNeptuneState,
    pub opensearch: fakecloud_opensearch::SharedOpenSearchState,
    pub pinpoint: fakecloud_pinpoint::SharedPinpointState,
    pub pipes: fakecloud_pipes::SharedPipesState,
    pub ram: fakecloud_ram::SharedRamState,
    pub redshift: fakecloud_redshift::SharedRedshiftState,
    pub resource_groups: fakecloud_resource_groups::SharedResourceGroupsState,
    pub resource_groups_tagging:
        fakecloud_resource_groups_tagging::SharedResourceGroupsTaggingState,
    pub s3tables: fakecloud_s3tables::SharedS3TablesState,
    pub sagemaker: fakecloud_sagemaker::SharedSageMakerState,
    pub serverlessrepo: fakecloud_serverlessrepo::SharedServerlessRepoState,
    pub servicediscovery: fakecloud_servicediscovery::SharedServiceDiscoveryState,
    pub shield: fakecloud_shield::SharedShieldState,
    pub ssoadmin: fakecloud_ssoadmin::SharedSsoAdminState,
    pub support: fakecloud_support::SharedSupportState,
    pub swf: fakecloud_swf::SharedSwfState,
    pub textract: fakecloud_textract::SharedTextractState,
    pub timestream: fakecloud_timestream::SharedTimestreamState,
    pub transcribe: fakecloud_transcribe::SharedTranscribeState,
    pub transfer: fakecloud_transfer::SharedTransferState,
    pub translate: fakecloud_translate::SharedTranslateState,
    pub verifiedpermissions: fakecloud_verifiedpermissions::SharedVerifiedPermissionsState,
    pub xray: fakecloud_xray::SharedXrayState,
    pub kafka_runtime: Option<Arc<fakecloud_kafka::KafkaRuntime>>,
    pub flink_runtime: Option<Arc<fakecloud_kinesisanalyticsv2::FlinkRuntime>>,
    pub mq_runtime: Option<Arc<fakecloud_mq::MqRuntime>>,
}

/// Accounts-map services (a `BTreeMap` of account id to state): a full reset
/// drops every account, an account reset that account.
macro_rules! accounts_map_reset {
    ($names:expr, $hook:expr, $state:expr, $ty:ty) => {{
        let all = $state.clone();
        let one = $state;
        ServiceReset::new(
            $names,
            $hook,
            move |_| *all.write() = <$ty>::default(),
            move |account_id, _| {
                one.write().accounts.remove(account_id);
            },
        )
    }};
}

/// The [`ServiceReset`] entries of the services [`ResetServiceStates`] holds.
pub(crate) fn reset_services(s: ResetServiceStates) -> Vec<ServiceReset> {
    let mut services = vec![
        ServiceReset::multi_account(&["account"], "account", s.account),
        ServiceReset::multi_account(&["amplify"], "amplify", s.amplify),
        ServiceReset::multi_account(&["appconfig"], "appconfig", s.appconfig),
        ServiceReset::multi_account(&["appsync"], "appsync", s.appsync),
        ServiceReset::multi_account(&["backup"], "backup", s.backup),
        ServiceReset::multi_account(&["ce"], "ce", s.ce),
        ServiceReset::multi_account(
            &["cloudcontrol", "cloudcontrolapi"],
            "cloudcontrol",
            s.cloudcontrol,
        ),
        ServiceReset::multi_account(&["cloudtrail"], "cloudtrail", s.cloudtrail),
        ServiceReset::multi_account(&["codeartifact"], "codeartifact", s.codeartifact),
        ServiceReset::multi_account(&["codebuild"], "codebuild", s.codebuild),
        ServiceReset::multi_account(&["codecommit"], "codecommit", s.codecommit),
        ServiceReset::multi_account(
            &["codeconnections", "codestar-connections"],
            "codeconnections",
            s.codeconnections,
        ),
        ServiceReset::multi_account(&["codedeploy"], "codedeploy", s.codedeploy),
        ServiceReset::multi_account(&["codepipeline"], "codepipeline", s.codepipeline),
        ServiceReset::multi_account(&["comprehend"], "comprehend", s.comprehend),
        ServiceReset::multi_account(&["dms"], "dms", s.dms),
        ServiceReset::multi_account(&["docdb"], "docdb", s.docdb),
        ServiceReset::multi_account(&["dsql"], "dsql", s.dsql),
        ServiceReset::multi_account(&["elasticfilesystem", "efs"], "elasticfilesystem", s.efs),
        ServiceReset::multi_account(&["eks"], "eks", s.eks),
        ServiceReset::multi_account(&["emr", "elasticmapreduce"], "emr", s.emr),
        ServiceReset::multi_account(&["fis"], "fis", s.fis),
        ServiceReset::multi_account(&["glacier"], "glacier", s.glacier),
        ServiceReset::multi_account(&["identitystore"], "identitystore", s.identitystore),
        ServiceReset::multi_account(&["iot"], "iot", s.iot),
        ServiceReset::multi_account(&["iotdata", "iot-data"], "iotdata", s.iotdata),
        ServiceReset::multi_account(&["iotwireless"], "iotwireless", s.iotwireless),
        ServiceReset::multi_account(&["lakeformation"], "lakeformation", s.lakeformation),
        ServiceReset::multi_account(
            &["managedblockchain"],
            "managedblockchain",
            s.managedblockchain,
        ),
        ServiceReset::multi_account(&["mediaconvert"], "mediaconvert", s.mediaconvert),
        ServiceReset::multi_account(&["memorydb"], "memorydb", s.memorydb),
        ServiceReset::multi_account(&["mwaa", "airflow"], "mwaa", s.mwaa),
        ServiceReset::multi_account(&["neptune"], "neptune", s.neptune),
        ServiceReset::multi_account(&["es", "opensearch"], "es", s.opensearch),
        ServiceReset::multi_account(&["pinpoint", "mobiletargeting"], "pinpoint", s.pinpoint),
        ServiceReset::multi_account(&["ram"], "ram", s.ram),
        ServiceReset::multi_account(&["resource-groups"], "resource-groups", s.resource_groups),
        ServiceReset::multi_account(
            &["resource-groups-tagging", "tagging"],
            "resource-groups-tagging",
            s.resource_groups_tagging,
        ),
        ServiceReset::multi_account(&["s3tables"], "s3tables", s.s3tables),
        ServiceReset::multi_account(&["sagemaker"], "sagemaker", s.sagemaker),
        ServiceReset::multi_account(&["serverlessrepo"], "serverlessrepo", s.serverlessrepo),
        ServiceReset::multi_account(
            &["servicediscovery"],
            "servicediscovery",
            s.servicediscovery,
        ),
        ServiceReset::multi_account(&["shield"], "shield", s.shield),
        ServiceReset::multi_account(&["ssoadmin", "sso"], "ssoadmin", s.ssoadmin),
        ServiceReset::multi_account(&["support"], "support", s.support),
        ServiceReset::multi_account(&["swf"], "swf", s.swf),
        ServiceReset::multi_account(&["textract"], "textract", s.textract),
        ServiceReset::multi_account(&["timestream"], "timestream", s.timestream),
        ServiceReset::multi_account(&["transcribe"], "transcribe", s.transcribe),
        ServiceReset::multi_account(&["transfer"], "transfer", s.transfer),
        ServiceReset::multi_account(&["translate"], "translate", s.translate),
        ServiceReset::multi_account(
            &["verifiedpermissions"],
            "verifiedpermissions",
            s.verifiedpermissions,
        ),
        ServiceReset::multi_account(&["xray"], "xray", s.xray),
        accounts_map_reset!(
            &["autoscaling"],
            "autoscaling",
            s.autoscaling,
            fakecloud_autoscaling::AutoScalingAccounts
        ),
        accounts_map_reset!(&["batch"], "batch", s.batch, fakecloud_batch::BatchAccounts),
        accounts_map_reset!(
            &["elasticbeanstalk"],
            "elasticbeanstalk",
            s.elasticbeanstalk,
            fakecloud_elasticbeanstalk::EbAccounts
        ),
        accounts_map_reset!(&["pipes"], "pipes", s.pipes, fakecloud_pipes::PipesAccounts),
        accounts_map_reset!(
            &["redshift"],
            "redshift",
            s.redshift,
            fakecloud_redshift::RedshiftAccounts
        ),
    ];
    services.push({
        let all = s.elbv2.clone();
        let one = s.elbv2;
        // The load balancers' listeners close on the data plane's next
        // reconcile, once their rows are gone.
        ServiceReset::new(
            &["elbv2", "elasticloadbalancing"],
            "elbv2",
            move |_| *all.write() = fakecloud_elbv2::Elbv2Accounts::new(),
            move |account_id, _| one.write().remove_account(account_id),
        )
    });
    services.push(mq_reset(s.mq, s.mq_runtime));
    services.push(kafka_reset(s.kafka, s.kafka_runtime));
    services.push(flink_reset(s.kinesisanalyticsv2, s.flink_runtime));
    services
}

/// Amazon MQ: the brokers' backing containers go with their rows.
fn mq_reset(
    state: fakecloud_mq::SharedMqState,
    runtime: Option<Arc<fakecloud_mq::MqRuntime>>,
) -> ServiceReset {
    let all = state.clone();
    let all_rt = runtime.clone();
    ServiceReset::new(
        &["mq"],
        "mq",
        move |teardown| {
            all.write().reset();
            if let Some(rt) = all_rt.clone() {
                teardown.push(async move { rt.stop_all().await });
            }
        },
        move |account_id, teardown| {
            let brokers: Vec<String> = {
                let mut mas = state.write();
                let brokers = mas
                    .get(account_id)
                    .map(|d| d.brokers.keys().cloned().collect())
                    .unwrap_or_default();
                mas.reset_account(account_id);
                brokers
            };
            if let Some(rt) = runtime.clone() {
                teardown.push(async move {
                    for broker in brokers {
                        rt.stop_broker(&broker).await;
                    }
                });
            }
        },
    )
}

/// Amazon MSK: the clusters' backing broker containers go with their rows.
fn kafka_reset(
    state: fakecloud_kafka::SharedKafkaState,
    runtime: Option<Arc<fakecloud_kafka::KafkaRuntime>>,
) -> ServiceReset {
    let all = state.clone();
    let all_rt = runtime.clone();
    ServiceReset::new(
        &["kafka", "msk"],
        "kafka",
        move |teardown| {
            all.write().reset();
            if let Some(rt) = all_rt.clone() {
                teardown.push(async move { rt.stop_all().await });
            }
        },
        move |account_id, teardown| {
            let clusters: Vec<String> = {
                let mut mas = state.write();
                let clusters = mas
                    .get(account_id)
                    .map(|d| d.clusters.keys().cloned().collect())
                    .unwrap_or_default();
                mas.reset_account(account_id);
                clusters
            };
            if let Some(rt) = runtime.clone() {
                teardown.push(async move {
                    for cluster in clusters {
                        rt.stop_broker(&cluster).await;
                    }
                });
            }
        },
    )
}

/// `(application ARN, backing container)` of every Flink application in an
/// account's state.
fn flink_apps(state: &fakecloud_kinesisanalyticsv2::Ka2State) -> Vec<(String, Option<String>)> {
    state
        .applications
        .values()
        .map(|app| {
            (
                app.arn.clone(),
                app.flink_binding.as_ref().map(|b| b.container_id.clone()),
            )
        })
        .collect()
}

/// Managed Service for Apache Flink: the applications' backing Flink
/// clusters go with their rows.
fn flink_reset(
    state: fakecloud_kinesisanalyticsv2::SharedKa2State,
    runtime: Option<Arc<fakecloud_kinesisanalyticsv2::FlinkRuntime>>,
) -> ServiceReset {
    fn teardown_apps(
        teardown: &mut Teardown,
        runtime: Option<Arc<fakecloud_kinesisanalyticsv2::FlinkRuntime>>,
        apps: Vec<(String, Option<String>)>,
    ) {
        if let Some(rt) = runtime {
            teardown.push(async move {
                for (arn, container) in apps {
                    rt.remove_cluster(&arn, container.as_deref()).await;
                }
            });
        }
    }
    let all = state.clone();
    let all_rt = runtime.clone();
    ServiceReset::new(
        &["kinesisanalyticsv2", "kinesisanalytics"],
        "kinesisanalyticsv2",
        move |teardown| {
            let apps = {
                let mut mas = all.write();
                let apps = mas.iter().flat_map(|(_, s)| flink_apps(s)).collect();
                mas.reset();
                apps
            };
            teardown_apps(teardown, all_rt.clone(), apps);
        },
        move |account_id, teardown| {
            let apps = {
                let mut mas = state.write();
                let apps = mas.get(account_id).map(flink_apps).unwrap_or_default();
                mas.reset_account(account_id);
                apps
            };
            teardown_apps(teardown, runtime.clone(), apps);
        },
    )
}

/// Bootstrap an IAM admin user in a specific account. Creates the user,
/// access key, and an inline admin policy (`Allow */*`) in the target
/// account's IAM state. Returns the credentials so the caller can sign
/// requests as that user.
///
/// This solves the multi-account bootstrap problem: the `test*` root
/// bypass only targets the default account, so there's no way to create
/// credentials for a non-default account via the normal AWS API.
///
/// The account is standalone unless `organization_id` names an existing
/// organization, in which case it is enrolled into that organization's
/// root OU. That mirrors AWS: a freshly vended account belongs to no
/// organization until it is invited and accepts, or is created through
/// `CreateAccount`. Bootstrapping an admin must never silently pull the
/// account into an unrelated organization — that account then inherits
/// SCPs it never agreed to, can read the organization's metadata, and
/// becomes a stack-set auto-deployment target.
pub(crate) fn create_admin_in_account(
    iam: &fakecloud_iam::SharedIamState,
    organizations: &fakecloud_organizations::SharedOrganizationsState,
    account_id: &str,
    user_name: &str,
    organization_id: Option<&str>,
) -> Result<types::CreateAdminResponse, CreateAdminError> {
    if let Some(org_id) = organization_id {
        let mut guard = organizations.write();
        if !guard.contains_org(org_id) {
            return Err(CreateAdminError::UnknownOrganization(org_id.to_string()));
        }
        // An account belongs to at most one organization. Without this the
        // shortcut would enroll it into a second registry entry, and which
        // organization's SCP ceiling, DescribeOrganization view and
        // stack-set targeting applied would come down to org-id sort order.
        // `CreateOrganization` and `InviteAccountToOrganization` both reject
        // this; so does the shortcut.
        if let Some(other) = guard.claimed_by_other_org(account_id, org_id) {
            return Err(CreateAdminError::AccountInAnotherOrganization {
                account_id: account_id.to_string(),
                organization_id: other,
            });
        }
        let org = guard
            .org_by_id_mut(org_id)
            .expect("checked just above that the organization exists");
        org.enroll_account_if_missing(account_id);
    }

    let mut accounts = iam.write();
    let region = accounts.region().to_string();
    let state = accounts.get_or_create(account_id);

    let user_id = format!(
        "AIDA{}",
        &uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .to_uppercase()[..16]
    );
    let arn = Arn::global_in(&region, "iam", account_id, &format!("user/{user_name}")).to_string();
    let akid = format!(
        "FKIA{}",
        &uuid::Uuid::new_v4()
            .to_string()
            .replace('-', "")
            .to_uppercase()[..20]
    );
    let secret = uuid::Uuid::new_v4().to_string();

    state.users.insert(
        user_name.to_string(),
        fakecloud_iam::IamUser {
            user_name: user_name.to_string(),
            user_id,
            arn: arn.clone(),
            path: "/".to_string(),
            created_at: chrono::Utc::now(),
            tags: Vec::new(),
            permissions_boundary: None,
        },
    );
    state.access_keys.insert(
        user_name.to_string(),
        vec![fakecloud_iam::IamAccessKey {
            access_key_id: akid.clone(),
            secret_access_key: secret.clone(),
            user_name: user_name.to_string(),
            status: "Active".to_string(),
            created_at: chrono::Utc::now(),
        }],
    );
    state.user_inline_policies.insert(
        user_name.to_string(),
        std::collections::BTreeMap::from([(
            "fakecloud-admin".to_string(),
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}"#.to_string(),
        )]),
    );

    Ok(types::CreateAdminResponse {
        access_key_id: akid,
        secret_access_key: secret,
        account_id: account_id.to_string(),
        arn,
    })
}

/// Why a `/_fakecloud/iam/create-admin` call could not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreateAdminError {
    /// `organizationId` was supplied but no organization with that id
    /// exists. Enrolling into "whatever org happens to exist" is what
    /// the caller is explicitly avoiding by naming one, so this is an
    /// error rather than a silent fallback.
    UnknownOrganization(String),
    /// The account is already a member of a different organization, and
    /// an account can only ever be in one.
    AccountInAnotherOrganization {
        account_id: String,
        organization_id: String,
    },
}

impl CreateAdminError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::UnknownOrganization(id) => {
                format!("no organization with id {id} exists")
            }
            Self::AccountInAnotherOrganization {
                account_id,
                organization_id,
            } => format!(
                "account {account_id} is already a member of organization {organization_id}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use chrono::Utc;
    use fakecloud_rds::{DbInstance, RdsState};

    use super::{LateReset, ResetState, ServiceReset, EXPLICIT_HOOKS};

    fn test_state() -> ResetState {
        ResetState {
            iam: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            sqs: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            sns: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            eb: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            ssm: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            dynamodb: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            lambda: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            secretsmanager: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            s3: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            logs: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            kms: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            cloudformation: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            ses: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            cognito: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            kinesis: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            rds: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            elasticache: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            ecr: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            ecs: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            stepfunctions: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            scheduler: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            apigatewayv1: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            apigatewayv2: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            bedrock: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "http://localhost:4566",
                ),
            )),
            bedrock_agent: Arc::new(parking_lot::RwLock::new(
                fakecloud_bedrock_agent::BedrockAgentAccounts::new(),
            )),
            bedrock_agent_runtime: Arc::new(parking_lot::RwLock::new(
                fakecloud_bedrock_agent_runtime::BedrockAgentRuntimeAccounts::new(),
            )),
            cloudfront: Arc::new(parking_lot::RwLock::new(
                fakecloud_cloudfront::CloudFrontAccounts::new(),
            )),
            route53: Arc::new(parking_lot::RwLock::new(
                fakecloud_route53::Route53Accounts::new(),
            )),
            acm: Arc::new(parking_lot::RwLock::new(fakecloud_acm::AcmAccounts::new())),
            acmpca: Arc::new(parking_lot::RwLock::new(
                fakecloud_acmpca::AcmPcaAccounts::new(),
            )),
            config: Arc::new(parking_lot::RwLock::new(
                fakecloud_config::ConfigAccounts::new(),
            )),
            route53resolver: Arc::new(parking_lot::RwLock::new(
                fakecloud_route53resolver::Route53ResolverAccounts::new(),
            )),
            firehose: Arc::new(parking_lot::RwLock::new(
                fakecloud_firehose::FirehoseAccounts::new(),
            )),
            glue: Arc::new(parking_lot::RwLock::new(fakecloud_glue::GlueAccounts::new())),
            cloudwatch: Arc::new(parking_lot::RwLock::new(
                fakecloud_cloudwatch::CloudWatchAccounts::new(),
            )),
            application_autoscaling: Arc::new(parking_lot::RwLock::new(
                fakecloud_application_autoscaling::ApplicationAutoScalingAccounts::new(),
            )),
            wafv2: Arc::new(parking_lot::RwLock::new(
                fakecloud_wafv2::Wafv2Accounts::new(),
            )),
            athena: Arc::new(parking_lot::RwLock::new(
                fakecloud_athena::AthenaAccounts::new(),
            )),
            organizations: Arc::new(parking_lot::RwLock::new(
                fakecloud_organizations::OrganizationsRegistry::default(),
            )),
            servicequotas: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            servicequotas_settings: Arc::new(parking_lot::RwLock::new(
                fakecloud_servicequotas::QuotaSettings::default(),
            )),
            servicequotas_baseline: fakecloud_servicequotas::QuotaSettings {
                enforce_all: true,
                ..Default::default()
            },
            container_runtime: None,
            rds_runtime: None,
            elasticache_runtime: None,
            ecs_runtime: None,
            ec2: Arc::new(parking_lot::RwLock::new(
                fakecloud_core::multi_account::MultiAccountState::new(
                    "123456789012",
                    "us-east-1",
                    "",
                ),
            )),
            ec2_runtime: None,
            late: Arc::new(OnceLock::new()),
        }
    }

    #[test]
    fn reset_service_clears_rds_state() {
        let mut rds_mas: fakecloud_core::multi_account::MultiAccountState<RdsState> =
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", "");
        let rds = rds_mas.default_mut();
        let created_at = Utc::now();
        rds.instances.insert(
            "db-1".to_string(),
            DbInstance {
                associated_roles: Vec::new(),
                db_instance_identifier: "db-1".to_string(),
                db_instance_arn: "arn:aws:rds:us-east-1:123456789012:db:db-1".to_string(),
                db_instance_class: "db.t3.micro".to_string(),
                engine: "postgres".to_string(),
                engine_version: "16.3".to_string(),
                db_instance_status: "available".to_string(),
                master_username: "admin".to_string(),
                db_name: Some("postgres".to_string()),
                db_subnet_group_name: None,
                endpoint_address: "127.0.0.1".to_string(),
                port: 5432,
                allocated_storage: 20,
                publicly_accessible: true,
                deletion_protection: false,
                created_at,
                dbi_resource_id: "db-test".to_string(),
                master_user_password: "secret123".to_string(),
                container_id: "container-id".to_string(),
                host_port: 15432,
                data_volume: None,
                tags: Vec::new(),
                read_replica_source_db_instance_identifier: None,
                read_replica_db_instance_identifiers: Vec::new(),
                vpc_security_group_ids: Vec::new(),
                db_parameter_group_name: None,
                backup_retention_period: 1,
                preferred_backup_window: "03:00-04:00".to_string(),
                preferred_maintenance_window: None,
                latest_restorable_time: Some(created_at),
                option_group_name: None,
                multi_az: false,
                pending_modified_values: None,
                availability_zone: None,
                storage_type: None,
                storage_encrypted: false,
                kms_key_id: None,
                iam_database_authentication_enabled: false,
                iops: None,
                monitoring_interval: None,
                monitoring_role_arn: None,
                performance_insights_enabled: false,
                performance_insights_kms_key_id: None,
                performance_insights_retention_period: None,
                enabled_cloudwatch_logs_exports: Vec::new(),
                ca_certificate_identifier: None,
                network_type: None,
                character_set_name: None,
                auto_minor_version_upgrade: None,
                copy_tags_to_snapshot: None,
                master_user_secret_arn: None,
                master_user_secret_kms_key_id: None,
                license_model: None,
                max_allocated_storage: None,
                multi_tenant: None,
                storage_throughput: None,
                tde_credential_arn: None,
                delete_automated_backups: None,
                db_security_groups: Vec::new(),
                domain: None,
                domain_fqdn: None,
                domain_ou: None,
                domain_iam_role_name: None,
                domain_auth_secret_arn: None,
                domain_dns_ips: Vec::new(),
                db_cluster_identifier: None,
                activity_stream: None,
            },
        );

        let state = ResetState {
            rds: Arc::new(parking_lot::RwLock::new(rds_mas)),
            ..test_state()
        };

        // Service Quotas: a reset drops applied values and restores the
        // startup enforcement settings.
        state
            .servicequotas
            .write()
            .get_or_create("123456789012")
            .applied
            .insert("us-east-1|vpc|L-2AFB9258".into(), 1.0);
        state.servicequotas_settings.write().enforce_all = false;
        state
            .reset_service("servicequotas")
            .expect("reset servicequotas");
        assert!(state
            .servicequotas
            .read()
            .get("123456789012")
            .is_some_and(|d| d.applied.is_empty()));
        assert!(state.servicequotas_settings.read().enforce_all);
        // A per-account reset keeps the organization template marker, so the
        // template is not applied to the account a second time.
        {
            let mut sq = state.servicequotas.write();
            let data = sq.get_or_create("123456789012");
            data.template_checked = Some("111111111111@marker".into());
            data.applied.insert("us-east-1|vpc|L-2AFB9258".into(), 9.0);
        }
        state
            .reset_service_for_account("servicequotas", "123456789012")
            .expect("reset servicequotas for account");
        {
            let sq = state.servicequotas.read();
            let data = sq.get("123456789012").unwrap();
            assert!(data.applied.is_empty());
            assert_eq!(
                data.template_checked.as_deref(),
                Some("111111111111@marker")
            );
        }

        state.reset_service("ec2").expect("reset ec2");
        state.reset_service("rds").expect("reset rds");

        assert!(state.rds.read().default_ref().instances.is_empty());
    }

    type Counts = Arc<parking_lot::Mutex<std::collections::BTreeMap<&'static str, usize>>>;

    /// A late wiring whose hooks count their runs, with SWF registered as a
    /// service reset through an entry.
    fn counting_late(swf: fakecloud_swf::SharedSwfState) -> (LateReset, Counts) {
        let counts: Counts = Arc::default();
        let mut late = LateReset::default();
        for key in EXPLICIT_HOOKS.iter().copied().chain(["swf"]) {
            let counts = counts.clone();
            let hook: fakecloud_persistence::SnapshotHook = Arc::new(move || {
                let counts = counts.clone();
                Box::pin(async move {
                    *counts.lock().entry(key).or_default() += 1;
                })
            });
            late.hooks.insert(key, hook);
        }
        late.services
            .push(ServiceReset::multi_account(&["swf"], "swf", swf));
        (late, counts)
    }

    fn swf_state() -> fakecloud_swf::SharedSwfState {
        Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ))
    }

    fn add_domain(swf: &fakecloud_swf::SharedSwfState, account: &str) {
        let mut mas = swf.write();
        let data = mas.get_or_create(account);
        data.domains.insert(
            "d".to_string(),
            fakecloud_swf::state::Domain {
                name: "d".to_string(),
                status: "REGISTERED".to_string(),
                description: None,
                retention_days: "1".to_string(),
                arn: "arn".to_string(),
            },
        );
    }

    fn domains(swf: &fakecloud_swf::SharedSwfState, account: &str) -> usize {
        swf.read().get(account).map_or(0, |d| d.domains.len())
    }

    #[tokio::test]
    async fn per_service_reset_persists_that_service() {
        let state = test_state();
        let (late, counts) = counting_late(swf_state());
        assert!(state.late.set(late).is_ok());

        let teardown = state.reset_service("events").unwrap();
        assert_eq!(teardown.persisted, vec!["eventbridge"]);
        teardown.run().await;
        let teardown = state.reset_service("apigateway").unwrap();
        assert_eq!(teardown.persisted, vec!["apigateway", "apigatewayv2"]);
        teardown.run().await;
        let teardown = state
            .reset_service_for_account("sqs", "123456789012")
            .unwrap();
        assert_eq!(teardown.persisted, vec!["sqs"]);
        teardown.run().await;

        let counts = counts.lock().clone();
        assert_eq!(counts.get("eventbridge"), Some(&1));
        assert_eq!(counts.get("apigateway"), Some(&1));
        assert_eq!(counts.get("apigatewayv2"), Some(&1));
        assert_eq!(counts.get("sqs"), Some(&1));
        assert_eq!(counts.len(), 4, "{counts:?}");
    }

    #[tokio::test]
    async fn full_reset_clears_registered_services_and_persists_everything() {
        let state = test_state();
        let swf = swf_state();
        add_domain(&swf, "123456789012");
        add_domain(&swf, "222222222222");
        let (late, counts) = counting_late(swf.clone());
        assert!(late.hooks_without_reset().is_empty());
        assert!(late.missing_hooks().is_empty());
        assert!(state.late.set(late).is_ok());

        let (_, teardown) = state.reset();
        teardown.run().await;
        assert_eq!(domains(&swf, "123456789012"), 0);
        assert_eq!(domains(&swf, "222222222222"), 0);
        let counts = counts.lock().clone();
        assert_eq!(counts.len(), EXPLICIT_HOOKS.len() + 1);
        assert!(counts.values().all(|n| *n == 1), "{counts:?}");
    }

    #[tokio::test]
    async fn registered_service_resets_per_service_and_per_account() {
        let state = test_state();
        let swf = swf_state();
        let (late, counts) = counting_late(swf.clone());
        assert!(state.late.set(late).is_ok());

        add_domain(&swf, "123456789012");
        add_domain(&swf, "222222222222");
        let teardown = state
            .reset_service_for_account("swf", "222222222222")
            .unwrap();
        assert_eq!(teardown.persisted, vec!["swf"]);
        teardown.run().await;
        assert_eq!(domains(&swf, "123456789012"), 1);
        assert_eq!(domains(&swf, "222222222222"), 0);

        state.reset_service("swf").unwrap().run().await;
        assert_eq!(domains(&swf, "123456789012"), 0);
        assert_eq!(counts.lock().get("swf"), Some(&2));

        assert!(state.reset_service("nope").is_err());
        assert!(state.reset_service_for_account("nope", "1").is_err());
    }

    #[test]
    fn late_reset_reports_unwired_hooks() {
        let (mut late, _) = counting_late(swf_state());
        let hook = late.hooks["sqs"].clone();
        late.hooks.insert("brand-new", hook);
        late.hooks.remove("rds");
        assert_eq!(late.hooks_without_reset(), vec!["brand-new"]);
        assert_eq!(late.missing_hooks(), vec!["rds"]);
    }

    #[tokio::test]
    async fn reset_deletes_s3_buckets_from_the_store() {
        use fakecloud_persistence::S3Store;
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fakecloud_persistence::s3::DiskS3Store::new(
            tmp.path().to_path_buf(),
            Arc::new(fakecloud_persistence::cache::BodyCache::new(0)),
        ));
        let meta = fakecloud_persistence::s3::BucketMeta {
            name: "kept".into(),
            ..Default::default()
        };
        store.put_bucket_meta("kept", &meta).unwrap();
        store.put_bucket_meta("gone", &meta).unwrap();
        let state = test_state();
        {
            let mut s3 = state.s3.write();
            for (account, bucket) in [("123456789012", "kept"), ("222222222222", "gone")] {
                s3.get_or_create(account).buckets.insert(
                    bucket.to_string(),
                    fakecloud_s3::S3Bucket::new(bucket, "us-east-1", account),
                );
            }
        }
        let late = LateReset {
            s3_store: Some(store.clone()),
            ..Default::default()
        };
        assert!(state.late.set(late).is_ok());

        state
            .reset_service_for_account("s3", "222222222222")
            .unwrap()
            .run()
            .await;
        assert!(store.bucket_state_exists("kept"));
        assert!(!store.bucket_state_exists("gone"));

        state.reset().1.run().await;
        assert!(!store.bucket_state_exists("kept"));
        assert!(state.s3.read().default_ref().buckets.is_empty());
    }

    #[test]
    fn create_admin_in_default_account() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "123456789012", "admin", None)
            .expect("create admin");
        assert_eq!(resp.account_id, "123456789012");
        assert!(resp.access_key_id.starts_with("FKIA"));
        assert!(resp.arn.contains("123456789012"));
        assert!(resp.arn.contains("admin"));

        // Verify state was populated
        let accounts = iam.read();
        let state = accounts.get("123456789012").unwrap();
        assert!(state.users.contains_key("admin"));
        assert!(state.access_keys.contains_key("admin"));
        assert!(state.user_inline_policies.contains_key("admin"));
    }

    #[test]
    fn create_admin_on_a_china_server_uses_the_aws_cn_partition() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "cn-north-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", None)
            .expect("create admin");
        assert_eq!(resp.arn, "arn:aws-cn:iam::222222222222:user/admin");
        assert_eq!(
            iam.read().get("222222222222").unwrap().users["admin"].arn,
            resp.arn
        );
    }

    #[test]
    fn create_admin_in_new_account() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "999999999999", "bob", None)
            .expect("create admin");
        assert_eq!(resp.account_id, "999999999999");
        assert!(resp.arn.contains("999999999999"));

        // New account was created
        let accounts = iam.read();
        assert!(accounts.get("999999999999").is_some());
        let state = accounts.get("999999999999").unwrap();
        assert!(state.users.contains_key("bob"));

        // Default account untouched
        let default = accounts.get("123456789012").unwrap();
        assert!(default.users.is_empty());
    }

    #[test]
    fn create_admin_policy_allows_all() {
        use fakecloud_core::auth::{
            ConditionContext, IamAction, IamDecision, IamPolicyEvaluator, Principal, PrincipalType,
        };
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", None)
            .expect("create admin");

        let evaluator = fakecloud_iam::policy_evaluator::IamPolicyEvaluatorImpl::new(iam.clone());
        let principal = Principal {
            arn: resp.arn.clone(),
            user_id: "AIDATEST".to_string(),
            account_id: "222222222222".to_string(),
            principal_type: PrincipalType::User,
            source_identity: None,
            tags: None,
        };
        let action = IamAction {
            service: "s3",
            action: "ListBuckets",
            resource: "*".to_string(),
        };
        let decision =
            evaluator.evaluate(&principal, &action, &ConditionContext::default(), &[], None);
        assert_eq!(
            decision,
            IamDecision::Allow,
            "admin policy should Allow */*"
        );
    }

    /// Regression for #2543: bootstrapping an admin must not silently
    /// pull the account into an organization someone else created. An
    /// auto-joined account cannot become a management account of its
    /// own, which broke multi-organization setups.
    #[test]
    fn create_admin_does_not_join_existing_organization() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(
                fakecloud_organizations::OrganizationState::bootstrap("111111111111").into(),
            ));

        super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", None)
            .expect("create admin");

        let guard = orgs.read();
        let org = guard.sole().unwrap();
        assert!(
            !org.accounts.contains_key("222222222222"),
            "a standalone bootstrap must leave the account outside the org"
        );
        assert!(org.accounts.contains_key("111111111111"));
    }

    #[test]
    fn create_admin_with_organization_id_enrolls_account() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let org = fakecloud_organizations::OrganizationState::bootstrap("111111111111");
        let org_id = org.org_id.clone();
        let root_id = org.root_id.clone();
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(org.into()));

        super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&org_id))
            .expect("create admin");

        let guard = orgs.read();
        let member = guard
            .sole()
            .unwrap()
            .accounts
            .get("222222222222")
            .expect("account enrolled");
        assert_eq!(member.parent_id, root_id);
        assert_eq!(member.status, "ACTIVE");
    }

    /// An account belongs to at most one organization. The bootstrap
    /// shortcut must reject a second enrollment rather than putting the
    /// account in two registries at once, where which organization's SCP
    /// ceiling and stack-set targeting applied would be arbitrary.
    #[test]
    fn create_admin_cannot_enroll_an_account_into_a_second_organization() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let first = fakecloud_organizations::OrganizationState::bootstrap("111111111111");
        let second = fakecloud_organizations::OrganizationState::bootstrap("999999999999");
        let first_id = first.org_id.clone();
        let second_id = second.org_id.clone();
        let mut registry = fakecloud_organizations::OrganizationsRegistry::default();
        registry.insert(first);
        registry.insert(second);
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(registry));

        super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&first_id))
            .expect("first enrollment");

        let err =
            super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&second_id))
                .expect_err("a second organization must be rejected");
        assert_eq!(
            err,
            super::CreateAdminError::AccountInAnotherOrganization {
                account_id: "222222222222".to_string(),
                organization_id: first_id.clone(),
            }
        );

        let guard = orgs.read();
        assert!(guard
            .org_by_id(&first_id)
            .unwrap()
            .accounts
            .contains_key("222222222222"));
        assert!(!guard
            .org_by_id(&second_id)
            .unwrap()
            .accounts
            .contains_key("222222222222"));
    }

    /// Re-naming the organization the account is already in is a no-op
    /// rather than an error — bootstrapping admin credentials twice for
    /// the same member must keep working.
    #[test]
    fn create_admin_into_the_account_s_own_organization_is_idempotent() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let org = fakecloud_organizations::OrganizationState::bootstrap("111111111111");
        let org_id = org.org_id.clone();
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(org.into()));

        for _ in 0..2 {
            super::create_admin_in_account(&iam, &orgs, "222222222222", "admin", Some(&org_id))
                .expect("repeat enrollment is a no-op");
        }
        assert_eq!(orgs.read().org_by_id(&org_id).unwrap().accounts.len(), 2);
    }

    #[test]
    fn create_admin_with_unknown_organization_id_errors() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState =
            Arc::new(parking_lot::RwLock::new(
                fakecloud_organizations::OrganizationState::bootstrap("111111111111").into(),
            ));

        let err = super::create_admin_in_account(
            &iam,
            &orgs,
            "222222222222",
            "admin",
            Some("o-doesnotexist"),
        )
        .expect_err("unknown org id must be rejected");
        assert_eq!(
            err,
            super::CreateAdminError::UnknownOrganization("o-doesnotexist".to_string())
        );
        // The IAM user is not created when the enrollment target is bogus.
        assert!(iam.read().get("222222222222").is_none());
    }

    #[test]
    fn create_admin_credentials_resolve() {
        let iam: fakecloud_iam::SharedIamState = Arc::new(parking_lot::RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new("123456789012", "us-east-1", ""),
        ));
        let orgs: fakecloud_organizations::SharedOrganizationsState = Arc::new(
            parking_lot::RwLock::new(fakecloud_organizations::OrganizationsRegistry::default()),
        );
        let resp = super::create_admin_in_account(&iam, &orgs, "222222222222", "alice", None)
            .expect("create admin");

        // Verify the credential resolver can find this key
        let mut accounts = iam.write();
        let state = accounts.get_or_create("222222222222");
        let lookup = state.credential_secret(&resp.access_key_id);
        assert!(lookup.is_some());
        let lookup = lookup.unwrap();
        assert_eq!(lookup.account_id, "222222222222");
        assert_eq!(lookup.secret_access_key, resp.secret_access_key);
    }
}
