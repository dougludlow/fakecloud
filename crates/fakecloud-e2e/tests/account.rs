//! AWS Account Management E2E over the real restJson1 wire format.
//!
//! `SendPhoneNumberVerification`, `VerifyPhoneNumber`, and
//! `GetContactInformationResponse.VerificationStatus` are newer than the typed
//! `aws-sdk-account` client, so this drives them over raw HTTP
//! (`POST /<operation>`). fakecloud sends no SMS: the one-time passcode is the
//! fixed, documented `000000`.

mod helpers;

use helpers::TestServer;
use serde_json::{json, Value};

async fn call(server: &TestServer, path: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/{path}", server.endpoint()))
        .header("content-type", "application/json")
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/account/aws4_request, SignedHeaders=host, Signature=0",
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

fn contact(phone: &str) -> Value {
    json!({
        "ContactInformation": {
            "FullName": "Jane Doe",
            "AddressLine1": "1 Main St",
            "City": "Seattle",
            "CountryCode": "US",
            "PhoneNumber": phone,
            "PostalCode": "98101"
        }
    })
}

#[tokio::test]
async fn phone_number_verification_lifecycle() {
    let server = TestServer::start().await;

    let (status, _) = call(&server, "putContactInformation", contact("+12065550100")).await;
    assert_eq!(status, 200);
    let (_, v) = call(&server, "getContactInformation", json!({})).await;
    assert_eq!(v["VerificationStatus"], json!("UNVERIFIED"), "{v}");
    assert_eq!(
        v["ContactInformation"]["PhoneNumber"],
        json!("+12065550100")
    );

    let (status, v) = call(&server, "sendPhoneNumberVerification", json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({ "Status": "PENDING" }));
    let (_, v) = call(&server, "getContactInformation", json!({})).await;
    assert_eq!(v["VerificationStatus"], json!("PENDING"));

    // A wrong passcode is rejected and the verification stays pending.
    let (status, _) = call(&server, "verifyPhoneNumber", json!({ "Otp": "999999" })).await;
    assert_eq!(status, 400);

    let (status, v) = call(&server, "verifyPhoneNumber", json!({ "Otp": "000000" })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({ "Status": "VERIFIED" }));
    let (_, v) = call(&server, "getContactInformation", json!({})).await;
    assert_eq!(v["VerificationStatus"], json!("VERIFIED"));

    // Changing the contact phone number resets verification.
    call(&server, "putContactInformation", contact("+12065550199")).await;
    let (_, v) = call(&server, "getContactInformation", json!({})).await;
    assert_eq!(v["VerificationStatus"], json!("UNVERIFIED"));
    let (status, _) = call(&server, "verifyPhoneNumber", json!({ "Otp": "000000" })).await;
    assert_eq!(status, 409);
}
