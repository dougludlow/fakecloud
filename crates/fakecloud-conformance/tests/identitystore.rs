mod helpers;

use fakecloud_conformance_macros::test_action;
use helpers::TestServer;

#[test_action("identitystore", "CreateGroup", checksum = "16519118")]
#[test_action("identitystore", "CreateGroupMembership", checksum = "7c4161f5")]
#[test_action("identitystore", "CreateUser", checksum = "8465952e")]
#[test_action("identitystore", "DeleteGroup", checksum = "fac5243d")]
#[test_action("identitystore", "DeleteGroupMembership", checksum = "be3816c5")]
#[test_action("identitystore", "DeleteUser", checksum = "a1616606")]
#[test_action("identitystore", "DescribeGroup", checksum = "5ba6a817")]
#[test_action("identitystore", "DescribeGroupMembership", checksum = "8d6d7ee8")]
#[test_action("identitystore", "DescribeUser", checksum = "7ace4b3d")]
#[test_action("identitystore", "GetGroupId", checksum = "9c842932")]
#[test_action("identitystore", "GetGroupMembershipId", checksum = "6b7a120d")]
#[test_action("identitystore", "GetUserId", checksum = "f73d0efd")]
#[test_action("identitystore", "IsMemberInGroups", checksum = "381b87c2")]
#[test_action("identitystore", "ListGroupMemberships", checksum = "0bae2350")]
#[test_action(
    "identitystore",
    "ListGroupMembershipsForMember",
    checksum = "066f8bb1"
)]
#[test_action("identitystore", "ListGroups", checksum = "e8c855e3")]
#[test_action("identitystore", "ListUsers", checksum = "0a857df7")]
#[test_action("identitystore", "UpdateGroup", checksum = "65e9ff88")]
#[test_action("identitystore", "UpdateUser", checksum = "22ce2a10")]
#[tokio::test]
async fn identitystore_probe() {
    let _server = TestServer::start().await;
}
