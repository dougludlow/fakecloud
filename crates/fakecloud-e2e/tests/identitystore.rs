//! IAM Identity Center Identity Store E2E over the real awsJson1.1 wire format
//! (x-amz-target `AWSIdentityStore.<Op>`).
//!
//! The identity-store resource operations (`ListIdentityStores`,
//! `DescribeIdentityStore`, `UpdateIdentityStore`) and the `Revision` /
//! resource-ARN members are newer than the typed `aws-sdk-identitystore`
//! client, so this drives them over raw HTTP. It proves the server wiring: the
//! identity store paired with the seeded SSO Admin instance is visible to the
//! Identity Store service before any directory write.

mod helpers;

use helpers::TestServer;
use serde_json::{json, Value};

async fn call(server: &TestServer, target: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(server.endpoint())
        .header("content-type", "application/x-amz-json-1.1")
        .header("x-amz-target", target)
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/identitystore/aws4_request, SignedHeaders=host, Signature=0",
        )
        .body(body.to_string())
        .send()
        .await
        .expect("request sent");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let v = if text.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("non-JSON body: {text}"))
    };
    (status, v)
}

async fn ids(server: &TestServer, op: &str, body: Value) -> Value {
    let (status, v) = call(server, &format!("AWSIdentityStore.{op}"), body).await;
    assert_eq!(status, 200, "{op}: {v}");
    v
}

#[tokio::test]
async fn identity_store_resource_follows_sso_admin_instance() {
    let server = TestServer::start().await;

    // The seeded IAM Identity Center instance names the account's store.
    let (status, instances) = call(&server, "SWBExternalService.ListInstances", json!({})).await;
    assert_eq!(status, 200, "{instances}");
    let instance = &instances["Instances"][0];
    let sid = instance["IdentityStoreId"].as_str().unwrap().to_string();
    let owner = instance["OwnerAccountId"].as_str().unwrap().to_string();
    let store_arn = format!("arn:aws:identitystore::{owner}:identitystore/{sid}");
    // Both services agree on the store's ARN.
    assert_eq!(instance["IdentityStoreArn"], json!(store_arn));

    let listed = ids(&server, "ListIdentityStores", json!({})).await;
    assert_eq!(
        listed["IdentityStores"],
        json!([{ "IdentityStoreId": sid, "IdentityStoreArn": store_arn }])
    );

    // Describe by ARN; no network configuration until one is set.
    let desc = ids(
        &server,
        "DescribeIdentityStore",
        json!({ "IdentityStoreId": store_arn }),
    )
    .await;
    assert_eq!(
        desc,
        json!({ "IdentityStoreId": sid, "IdentityStoreArn": store_arn })
    );

    let updated = ids(
        &server,
        "UpdateIdentityStore",
        json!({
            "IdentityStoreId": sid,
            "NetworkConfiguration": {
                "VpceAccessRequired": true,
                "ScimAllowSourceIps": ["0.0.0.0/0"]
            }
        }),
    )
    .await;
    assert_eq!(updated["IdentityStoreArn"], json!(store_arn));
    let desc = ids(
        &server,
        "DescribeIdentityStore",
        json!({ "IdentityStoreId": sid }),
    )
    .await;
    assert_eq!(
        desc["NetworkConfiguration"],
        json!({ "VpceAccessRequired": true, "ScimAllowSourceIps": ["0.0.0.0/0"] })
    );

    // An unknown store is a 404 naming the IDENTITY_STORE resource type.
    let (status, err) = call(
        &server,
        "AWSIdentityStore.DescribeIdentityStore",
        json!({ "IdentityStoreId": "d-ffffffffff" }),
    )
    .await;
    assert_eq!(status, 404, "{err}");
    assert_eq!(err["ResourceType"], json!("IDENTITY_STORE"));
}

#[tokio::test]
async fn user_revision_and_arn_round_trip() {
    let server = TestServer::start().await;
    let sid = "d-0123456789";

    let created = ids(
        &server,
        "CreateUser",
        json!({ "IdentityStoreId": sid, "UserName": "alice", "DisplayName": "Alice" }),
    )
    .await;
    let uid = created["UserId"].as_str().unwrap().to_string();
    assert!(uid.starts_with("0123456789-"), "{uid}");
    let user_arn = format!("arn:aws:identitystore:::user/{uid}");
    assert_eq!(created["UserArn"], json!(user_arn));
    assert_eq!(created["Revision"], json!("1"));

    let updated = ids(
        &server,
        "UpdateUser",
        json!({
            "IdentityStoreId": sid, "UserId": uid, "Revision": "1",
            "Operations": [{ "AttributePath": "displayName", "AttributeValue": "Alice B" }]
        }),
    )
    .await;
    assert_eq!(
        updated,
        json!({ "IdentityStoreId": sid, "UserId": uid, "UserArn": user_arn, "Revision": "2" })
    );

    // A stale revision is a 409 ConflictException with CONCURRENT_MODIFICATION.
    let (status, err) = call(
        &server,
        "AWSIdentityStore.DeleteUser",
        json!({ "IdentityStoreId": sid, "UserId": uid, "Revision": "1" }),
    )
    .await;
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["Reason"], json!("CONCURRENT_MODIFICATION"));

    let desc = ids(
        &server,
        "DescribeUser",
        json!({ "IdentityStoreId": sid, "UserId": uid }),
    )
    .await;
    assert_eq!(desc["DisplayName"], json!("Alice B"));
    assert_eq!(desc["Revision"], json!("2"));
    assert_eq!(desc["UserArn"], json!(user_arn));

    ids(
        &server,
        "DeleteUser",
        json!({ "IdentityStoreId": sid, "UserId": uid, "Revision": "2" }),
    )
    .await;

    // The directory created by the write is now a listed store too.
    let listed = ids(&server, "ListIdentityStores", json!({})).await;
    assert!(
        listed["IdentityStores"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["IdentityStoreId"] == json!(sid)),
        "{listed}"
    );
}
