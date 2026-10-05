//! Service Quotas E2E.
//!
//! Drives the Service Quotas API with the AWS Rust SDK (awsJson1.1,
//! x-amz-target `ServiceQuotasV20190624.<Op>`) and checks that the applied
//! values it reports are the ones EC2 enforces once enforcement is switched
//! on: raising "Inbound or outbound rules per security group" or "Security
//! groups per network interface" with `RequestServiceQuotaIncrease` changes
//! what `AuthorizeSecurityGroupIngress` and `CreateNetworkInterface` accept.
//! Enforcement is opt-in (`--enforce-quotas`, `--enforce-quota`, or the
//! `/_fakecloud/service-quotas/*` introspection API, driven here through the
//! Rust `fakecloud-sdk`).

mod helpers;

use aws_sdk_ec2::types::{IpPermission, IpRange};
use aws_sdk_servicequotas::error::ProvideErrorMetadata;
use aws_sdk_servicequotas::types::{RequestStatus, Tag};
use fakecloud_sdk::types::{
    PutQuotaEnforcementRequest, PutServiceQuotaRequest, QuotaEnforcement, QuotaOverrideChange,
};
use fakecloud_sdk::FakeCloud;
use helpers::TestServer;

const VPC: &str = "vpc";
const SGS_PER_ENI: &str = "L-2AFB9258";
const RULES_PER_SG: &str = "L-0EA8095F";

async fn clients(server: &TestServer) -> (aws_sdk_servicequotas::Client, aws_sdk_ec2::Client) {
    let config = server.aws_config().await;
    (
        aws_sdk_servicequotas::Client::new(&config),
        aws_sdk_ec2::Client::new(&config),
    )
}

async fn applied(sq: &aws_sdk_servicequotas::Client, code: &str) -> f64 {
    sq.get_service_quota()
        .service_code(VPC)
        .quota_code(code)
        .send()
        .await
        .unwrap()
        .quota()
        .unwrap()
        .value()
        .unwrap()
}

async fn request_increase(sq: &aws_sdk_servicequotas::Client, code: &str, value: f64) -> String {
    let resp = sq
        .request_service_quota_increase()
        .service_code(VPC)
        .quota_code(code)
        .desired_value(value)
        .send()
        .await
        .unwrap();
    let req = resp.requested_quota().unwrap();
    assert_eq!(req.status(), Some(&RequestStatus::Pending));
    req.id().unwrap().to_string()
}

async fn status(sq: &aws_sdk_servicequotas::Client, id: &str) -> RequestStatus {
    sq.get_requested_service_quota_change()
        .request_id(id)
        .send()
        .await
        .unwrap()
        .requested_quota()
        .unwrap()
        .status()
        .unwrap()
        .clone()
}

fn cidr_rule(n: usize) -> IpPermission {
    IpPermission::builder()
        .ip_protocol("tcp")
        .from_port(1000 + n as i32)
        .to_port(1000 + n as i32)
        .ip_ranges(IpRange::builder().cidr_ip("10.0.0.0/8").build())
        .build()
}

#[tokio::test]
async fn catalog_lookups() {
    let server = TestServer::start().await;
    let (sq, _) = clients(&server).await;

    let services = sq.list_services().send().await.unwrap();
    assert!(services
        .services()
        .iter()
        .any(|s| s.service_code() == Some(VPC)));

    let default = sq
        .get_aws_default_service_quota()
        .service_code(VPC)
        .quota_code(SGS_PER_ENI)
        .send()
        .await
        .unwrap();
    let q = default.quota().unwrap();
    assert_eq!(q.value(), Some(5.0));
    assert_eq!(
        q.quota_name(),
        Some("Security groups per network interface")
    );
    assert_eq!(
        q.quota_arn(),
        Some("arn:aws:servicequotas:us-east-1::vpc/L-2AFB9258")
    );

    let all = sq
        .list_service_quotas()
        .service_code(VPC)
        .send()
        .await
        .unwrap();
    assert!(all
        .quotas()
        .iter()
        .any(|q| q.quota_code() == Some(RULES_PER_SG) && q.value() == Some(60.0)));

    let err = sq
        .get_service_quota()
        .service_code(VPC)
        .quota_code("L-00000000")
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("NoSuchResourceException"));
}

#[tokio::test]
async fn raised_rule_quota_is_enforced_by_ec2() {
    let server = TestServer::start_full(&[], &["--enforce-quota", "vpc/L-0EA8095F"]).await;
    let (sq, ec2) = clients(&server).await;

    let vpc = ec2
        .create_vpc()
        .cidr_block("10.0.0.0/16")
        .send()
        .await
        .unwrap();
    let vpc_id = vpc.vpc().unwrap().vpc_id().unwrap().to_string();
    let sg = ec2
        .create_security_group()
        .group_name("quota")
        .description("quota")
        .vpc_id(&vpc_id)
        .send()
        .await
        .unwrap();
    let sg_id = sg.group_id().unwrap().to_string();

    // The AWS default allows 60 inbound rules.
    ec2.authorize_security_group_ingress()
        .group_id(&sg_id)
        .set_ip_permissions(Some((0..60).map(cidr_rule).collect()))
        .send()
        .await
        .unwrap();
    let err = ec2
        .authorize_security_group_ingress()
        .group_id(&sg_id)
        .ip_permissions(cidr_rule(60))
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        aws_sdk_ec2::error::ProvideErrorMetadata::code(&err),
        Some("RulesPerSecurityGroupLimitExceeded")
    );

    // 5 groups per interface x 100 rules = 500 <= 1000: approved.
    let id = request_increase(&sq, RULES_PER_SG, 100.0).await;
    assert_eq!(status(&sq, &id).await, RequestStatus::Approved);
    assert_eq!(applied(&sq, RULES_PER_SG).await, 100.0);

    ec2.authorize_security_group_ingress()
        .group_id(&sg_id)
        .ip_permissions(cidr_rule(60))
        .send()
        .await
        .unwrap();

    let history = sq
        .list_requested_service_quota_change_history_by_quota()
        .service_code(VPC)
        .quota_code(RULES_PER_SG)
        .send()
        .await
        .unwrap();
    assert_eq!(history.requested_quotas().len(), 1);
}

#[tokio::test]
async fn raised_groups_per_interface_quota_is_enforced_by_ec2() {
    let server = TestServer::start_full(&[], &["--enforce-quotas"]).await;
    let (sq, ec2) = clients(&server).await;

    let vpc = ec2
        .create_vpc()
        .cidr_block("10.1.0.0/16")
        .send()
        .await
        .unwrap();
    let vpc_id = vpc.vpc().unwrap().vpc_id().unwrap().to_string();
    let subnet = ec2
        .create_subnet()
        .vpc_id(&vpc_id)
        .cidr_block("10.1.1.0/24")
        .send()
        .await
        .unwrap();
    let subnet_id = subnet.subnet().unwrap().subnet_id().unwrap().to_string();
    let mut groups = Vec::new();
    for i in 0..6 {
        let sg = ec2
            .create_security_group()
            .group_name(format!("g{i}"))
            .description("g")
            .vpc_id(&vpc_id)
            .send()
            .await
            .unwrap();
        groups.push(sg.group_id().unwrap().to_string());
    }

    let err = ec2
        .create_network_interface()
        .subnet_id(&subnet_id)
        .set_groups(Some(groups.clone()))
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        aws_sdk_ec2::error::ProvideErrorMetadata::code(&err),
        Some("SecurityGroupsPerInterfaceLimitExceeded")
    );

    // 8 groups x 60 rules = 480: approved.
    let id = request_increase(&sq, SGS_PER_ENI, 8.0).await;
    assert_eq!(status(&sq, &id).await, RequestStatus::Approved);
    ec2.create_network_interface()
        .subnet_id(&subnet_id)
        .set_groups(Some(groups))
        .send()
        .await
        .unwrap();

    // 8 groups x 200 rules = 1600 > 1000: AWS does not approve it, and the
    // applied value stays.
    let id = request_increase(&sq, RULES_PER_SG, 200.0).await;
    assert_eq!(status(&sq, &id).await, RequestStatus::NotApproved);
    assert_eq!(applied(&sq, RULES_PER_SG).await, 60.0);
}

#[tokio::test]
async fn applied_quota_tags_and_utilization_report() {
    let server = TestServer::start().await;
    let (sq, ec2) = clients(&server).await;

    let arn = sq
        .get_service_quota()
        .service_code(VPC)
        .quota_code(SGS_PER_ENI)
        .send()
        .await
        .unwrap()
        .quota()
        .unwrap()
        .quota_arn()
        .unwrap()
        .to_string();
    sq.tag_resource()
        .resource_arn(&arn)
        .tags(Tag::builder().key("team").value("network").build().unwrap())
        .send()
        .await
        .unwrap();
    let tags = sq
        .list_tags_for_resource()
        .resource_arn(&arn)
        .send()
        .await
        .unwrap();
    assert_eq!(tags.tags().len(), 1);
    assert_eq!(tags.tags()[0].value(), "network");

    let before = ec2.describe_vpcs().send().await.unwrap().vpcs().len();
    ec2.create_vpc()
        .cidr_block("10.2.0.0/16")
        .send()
        .await
        .unwrap();
    let report = sq.start_quota_utilization_report().send().await.unwrap();
    let got = sq
        .get_quota_utilization_report()
        .report_id(report.report_id().unwrap())
        .send()
        .await
        .unwrap();
    let vpcs = got
        .quotas()
        .iter()
        .find(|q| q.quota_code() == Some("L-F678F1CE"))
        .expect("VPCs per Region is measured");
    let expected = (before + 1) as f64 / 5.0 * 100.0;
    assert_eq!(vpcs.utilization(), Some(expected));
}

/// A VPC, a subnet and `n` security groups in it.
async fn vpc_with_groups(ec2: &aws_sdk_ec2::Client, n: usize) -> (String, String, Vec<String>) {
    let vpc = ec2
        .create_vpc()
        .cidr_block("10.9.0.0/16")
        .send()
        .await
        .unwrap();
    let vpc_id = vpc.vpc().unwrap().vpc_id().unwrap().to_string();
    let subnet = ec2
        .create_subnet()
        .vpc_id(&vpc_id)
        .cidr_block("10.9.1.0/24")
        .send()
        .await
        .unwrap();
    let subnet_id = subnet.subnet().unwrap().subnet_id().unwrap().to_string();
    let mut groups = Vec::new();
    for i in 0..n {
        let sg = ec2
            .create_security_group()
            .group_name(format!("g{i}"))
            .description("g")
            .vpc_id(&vpc_id)
            .send()
            .await
            .unwrap();
        groups.push(sg.group_id().unwrap().to_string());
    }
    (vpc_id, subnet_id, groups)
}

async fn eni_error(
    ec2: &aws_sdk_ec2::Client,
    subnet_id: &str,
    groups: &[String],
) -> Option<String> {
    ec2.create_network_interface()
        .subnet_id(subnet_id)
        .set_groups(Some(groups.to_vec()))
        .send()
        .await
        .err()
        .and_then(|e| aws_sdk_ec2::error::ProvideErrorMetadata::code(&e).map(str::to_string))
}

#[tokio::test]
async fn quotas_are_not_enforced_by_default() {
    let server = TestServer::start().await;
    let (_, ec2) = clients(&server).await;
    let (_, subnet_id, groups) = vpc_with_groups(&ec2, 6).await;
    // Six groups on one interface: past the AWS default of 5, accepted.
    assert_eq!(eni_error(&ec2, &subnet_id, &groups).await, None);
    // 61 inbound rules: past the AWS default of 60, accepted.
    ec2.authorize_security_group_ingress()
        .group_id(&groups[0])
        .set_ip_permissions(Some((0..61).map(cidr_rule).collect()))
        .send()
        .await
        .unwrap();

    let fc = FakeCloud::new(server.endpoint());
    let quotas = fc
        .service_quotas()
        .get_quotas(None, None, Some(VPC))
        .await
        .unwrap();
    let rules = quotas
        .quotas
        .iter()
        .find(|q| q.quota_code == RULES_PER_SG)
        .unwrap();
    assert!(rules.enforceable);
    assert!(!rules.enforced);
    assert_eq!(rules.enforcement_source, "global");
    let vpcs = quotas
        .quotas
        .iter()
        .find(|q| q.quota_code == "L-F678F1CE")
        .unwrap();
    assert!(vpcs.usage.is_some_and(|u| u >= 1.0), "{:?}", vpcs.usage);
}

#[tokio::test]
async fn introspection_lowers_and_enforces_a_quota() {
    let server = TestServer::start().await;
    let (sq, ec2) = clients(&server).await;
    let fc = FakeCloud::new(server.endpoint());
    let (_, subnet_id, groups) = vpc_with_groups(&ec2, 2).await;

    // One group per interface, below the AWS default of 5 (AWS never lowers
    // a quota; a test can, to hit the limit without five groups).
    let quota = fc
        .service_quotas()
        .put_quota(
            VPC,
            SGS_PER_ENI,
            &PutServiceQuotaRequest {
                value: Some(1.0),
                enforce: Some(QuotaEnforcement::Enforce),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(quota.applied_value, 1.0);
    assert!(quota.enforced);
    assert_eq!(quota.enforcement_source, "override");
    // The Service Quotas API reports the same applied value.
    assert_eq!(applied(&sq, SGS_PER_ENI).await, 1.0);
    assert_eq!(
        eni_error(&ec2, &subnet_id, &groups).await.as_deref(),
        Some("SecurityGroupsPerInterfaceLimitExceeded")
    );
    assert_eq!(eni_error(&ec2, &subnet_id, &groups[..1]).await, None);

    // Back to the default, and no longer enforced.
    let quota = fc
        .service_quotas()
        .delete_quota(VPC, SGS_PER_ENI, None, None)
        .await
        .unwrap();
    assert_eq!(quota.applied_value, 5.0);
    assert!(!quota.enforced);
    assert_eq!(eni_error(&ec2, &subnet_id, &groups).await, None);

    // Unknown quotas and quotas no service checks are refused.
    let err = fc
        .service_quotas()
        .put_quota(
            VPC,
            "L-NOPE",
            &PutServiceQuotaRequest {
                value: Some(1.0),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, fakecloud_sdk::Error::Api { status: 404, .. }),
        "{err}"
    );
    let err = fc
        .service_quotas()
        .put_quota(
            "lambda",
            "L-B99A9384",
            &PutServiceQuotaRequest {
                enforce: Some(QuotaEnforcement::Enforce),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, fakecloud_sdk::Error::Api { status: 400, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn global_switch_with_a_per_quota_ignore() {
    let server = TestServer::start().await;
    let (_, ec2) = clients(&server).await;
    let fc = FakeCloud::new(server.endpoint());
    let enforcement = fc
        .service_quotas()
        .put_enforcement(&PutQuotaEnforcementRequest {
            enforce_all: Some(true),
            overrides: vec![QuotaOverrideChange {
                service_code: VPC.into(),
                quota_code: RULES_PER_SG.into(),
                account_id: None,
                enforce: QuotaEnforcement::Ignore,
            }],
        })
        .await
        .unwrap();
    assert!(enforcement.enforce_all);
    assert_eq!(enforcement.overrides.len(), 1);
    assert!(!enforcement.overrides[0].enforce);

    let (_, subnet_id, groups) = vpc_with_groups(&ec2, 6).await;
    assert_eq!(
        eni_error(&ec2, &subnet_id, &groups).await.as_deref(),
        Some("SecurityGroupsPerInterfaceLimitExceeded")
    );
    ec2.authorize_security_group_ingress()
        .group_id(&groups[0])
        .set_ip_permissions(Some((0..61).map(cidr_rule).collect()))
        .send()
        .await
        .unwrap();

    // An account-scoped override applies to that account only.
    let other = "210987654321";
    let enforcement = fc
        .service_quotas()
        .put_enforcement(&PutQuotaEnforcementRequest {
            enforce_all: None,
            overrides: vec![QuotaOverrideChange {
                service_code: VPC.into(),
                quota_code: SGS_PER_ENI.into(),
                account_id: Some(other.into()),
                enforce: QuotaEnforcement::Ignore,
            }],
        })
        .await
        .unwrap();
    assert_eq!(
        enforcement.account_overrides[0].account_id.as_deref(),
        Some(other)
    );
    let quotas = fc
        .service_quotas()
        .get_quotas(Some(other), None, Some(VPC))
        .await
        .unwrap();
    let q = quotas
        .quotas
        .iter()
        .find(|q| q.quota_code == SGS_PER_ENI)
        .unwrap();
    assert!(!q.enforced);
    assert_eq!(q.enforcement_source, "account_override");
}

#[tokio::test]
async fn manual_approval_holds_requests_until_decided() {
    let server = TestServer::start_full(&[], &["--quota-requests", "manual"]).await;
    let (sq, _) = clients(&server).await;
    let fc = FakeCloud::new(server.endpoint());
    assert_eq!(
        fc.service_quotas()
            .get_request_approval()
            .await
            .unwrap()
            .mode,
        "manual"
    );

    let id = request_increase(&sq, RULES_PER_SG, 100.0).await;
    assert_eq!(status(&sq, &id).await, RequestStatus::Pending);
    assert_eq!(applied(&sq, RULES_PER_SG).await, 60.0);
    // A pending request can open a support case, as on AWS.
    sq.create_support_case()
        .request_id(&id)
        .send()
        .await
        .unwrap();
    assert_eq!(status(&sq, &id).await, RequestStatus::CaseOpened);

    let open = fc
        .service_quotas()
        .get_requests(None, Some("CASE_OPENED"))
        .await
        .unwrap();
    assert_eq!(open.requests.len(), 1);
    assert_eq!(open.requests[0].request_id, id);
    assert!(open.requests[0].case_id.is_some());

    let decided = fc.service_quotas().approve_request(&id).await.unwrap();
    assert_eq!(decided.status, "APPROVED");
    assert_eq!(status(&sq, &id).await, RequestStatus::Approved);
    assert_eq!(applied(&sq, RULES_PER_SG).await, 100.0);

    let id = request_increase(&sq, SGS_PER_ENI, 8.0).await;
    let decided = fc
        .service_quotas()
        .deny_request(&id, Some("DENIED"))
        .await
        .unwrap();
    assert_eq!(decided.status, "DENIED");
    assert_eq!(status(&sq, &id).await, RequestStatus::Denied);
    assert_eq!(applied(&sq, SGS_PER_ENI).await, 5.0);
    let err = fc.service_quotas().approve_request(&id).await.unwrap_err();
    assert!(
        matches!(err, fakecloud_sdk::Error::Api { status: 409, .. }),
        "{err}"
    );

    // Back to automatic approval: the next request is decided on submission.
    fc.service_quotas()
        .set_request_approval("auto")
        .await
        .unwrap();
    let id = request_increase(&sq, SGS_PER_ENI, 8.0).await;
    assert_eq!(status(&sq, &id).await, RequestStatus::Approved);
}

#[tokio::test]
async fn reset_restores_the_startup_quota_settings() {
    let server = TestServer::start_full(&[], &["--enforce-quotas"]).await;
    let fc = FakeCloud::new(server.endpoint());
    fc.service_quotas()
        .put_enforcement(&PutQuotaEnforcementRequest {
            enforce_all: Some(false),
            overrides: vec![],
        })
        .await
        .unwrap();
    fc.service_quotas()
        .put_quota(
            VPC,
            SGS_PER_ENI,
            &PutServiceQuotaRequest {
                value: Some(1.0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    fc.reset().await.unwrap();
    assert!(
        fc.service_quotas()
            .get_enforcement()
            .await
            .unwrap()
            .enforce_all
    );
    let quotas = fc
        .service_quotas()
        .get_quotas(None, None, Some(VPC))
        .await
        .unwrap();
    let q = quotas
        .quotas
        .iter()
        .find(|q| q.quota_code == SGS_PER_ENI)
        .unwrap();
    assert_eq!(q.applied_value, 5.0);
}

#[tokio::test]
async fn quota_settings_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().display().to_string();
    let mut server = TestServer::start_full(
        &[("FAKECLOUD_CONTAINER_CLI", "false")],
        &["--storage-mode", "persistent", "--data-path", &data],
    )
    .await;
    let fc = FakeCloud::new(server.endpoint());
    fc.service_quotas()
        .put_quota(
            VPC,
            SGS_PER_ENI,
            &PutServiceQuotaRequest {
                value: Some(2.0),
                enforce: Some(QuotaEnforcement::Enforce),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    fc.service_quotas()
        .set_request_approval("manual")
        .await
        .unwrap();

    server.restart().await;
    let fc = FakeCloud::new(server.endpoint());
    let quotas = fc
        .service_quotas()
        .get_quotas(None, None, Some(VPC))
        .await
        .unwrap();
    let q = quotas
        .quotas
        .iter()
        .find(|q| q.quota_code == SGS_PER_ENI)
        .unwrap();
    assert_eq!(q.applied_value, 2.0);
    assert!(q.enforced);
    assert_eq!(
        fc.service_quotas()
            .get_request_approval()
            .await
            .unwrap()
            .mode,
        "manual"
    );
}
