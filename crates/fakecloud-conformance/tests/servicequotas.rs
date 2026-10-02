//! Conformance coverage marker for Service Quotas. The behavior is exercised
//! by the unit tests in `fakecloud-servicequotas` and the `servicequotas` E2E.

mod helpers;

use fakecloud_conformance_macros::test_action;
use helpers::TestServer;

#[test_action(
    "servicequotas",
    "AssociateServiceQuotaTemplate",
    checksum = "ae31066a"
)]
#[test_action("servicequotas", "CreateSupportCase", checksum = "61d6f8b7")]
#[test_action(
    "servicequotas",
    "DeleteServiceQuotaIncreaseRequestFromTemplate",
    checksum = "5e26de85"
)]
#[test_action(
    "servicequotas",
    "DisassociateServiceQuotaTemplate",
    checksum = "796b69b3"
)]
#[test_action("servicequotas", "GetAWSDefaultServiceQuota", checksum = "a85db35c")]
#[test_action(
    "servicequotas",
    "GetAssociationForServiceQuotaTemplate",
    checksum = "690592af"
)]
#[test_action(
    "servicequotas",
    "GetAutoManagementConfiguration",
    checksum = "30bebae3"
)]
#[test_action("servicequotas", "GetQuotaUtilizationReport", checksum = "aaa7f0b7")]
#[test_action(
    "servicequotas",
    "GetRequestedServiceQuotaChange",
    checksum = "7291f4eb"
)]
#[test_action("servicequotas", "GetServiceQuota", checksum = "1cfac062")]
#[test_action(
    "servicequotas",
    "GetServiceQuotaIncreaseRequestFromTemplate",
    checksum = "8657ac09"
)]
#[test_action("servicequotas", "ListAWSDefaultServiceQuotas", checksum = "5d70f03d")]
#[test_action(
    "servicequotas",
    "ListRequestedServiceQuotaChangeHistory",
    checksum = "76b94e59"
)]
#[test_action(
    "servicequotas",
    "ListRequestedServiceQuotaChangeHistoryByQuota",
    checksum = "731db897"
)]
#[test_action(
    "servicequotas",
    "ListServiceQuotaIncreaseRequestsInTemplate",
    checksum = "d73e285c"
)]
#[test_action("servicequotas", "ListServiceQuotas", checksum = "5f67fba5")]
#[test_action("servicequotas", "ListServices", checksum = "c58dcf67")]
#[test_action("servicequotas", "ListTagsForResource", checksum = "b6f2d0ab")]
#[test_action(
    "servicequotas",
    "PutServiceQuotaIncreaseRequestIntoTemplate",
    checksum = "f3477f1c"
)]
#[test_action("servicequotas", "RequestServiceQuotaIncrease", checksum = "18b832e5")]
#[test_action("servicequotas", "StartAutoManagement", checksum = "5178a84a")]
#[test_action("servicequotas", "StartQuotaUtilizationReport", checksum = "f51b9ed5")]
#[test_action("servicequotas", "StopAutoManagement", checksum = "ee816ee0")]
#[test_action("servicequotas", "TagResource", checksum = "d85617b7")]
#[test_action("servicequotas", "UntagResource", checksum = "68786784")]
#[test_action("servicequotas", "UpdateAutoManagement", checksum = "da46388a")]
#[tokio::test]
async fn servicequotas_probe() {
    let _server = TestServer::start().await;
}
