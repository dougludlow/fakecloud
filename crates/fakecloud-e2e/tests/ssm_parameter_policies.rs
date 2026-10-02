//! E2E tests for SSM Parameter Store policies (I2): Expiration,
//! ExpirationNotification, NoChangeNotification. Verifies lazy
//! deletion + notification fan-out via the
//! `/_fakecloud/ssm/parameter-policy-events` admin endpoint.

mod helpers;

use aws_sdk_ssm::types::{ParameterTier, ParameterType};
use helpers::TestServer;
use serde_json::json;

fn policies_json(items: serde_json::Value) -> String {
    serde_json::to_string(&items).unwrap()
}

async fn fetch_policy_events(server: &TestServer) -> serde_json::Value {
    let http = reqwest::Client::new();
    let resp = http
        .get(format!(
            "{}/_fakecloud/ssm/parameter-policy-events",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "admin endpoint failed");
    resp.json().await.unwrap()
}

#[tokio::test]
async fn ssm_expiration_in_past_deletes_on_get() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    let past = "1990-01-01T00:00:00.000Z";
    let policies = policies_json(json!([{
        "Type": "Expiration",
        "Version": "1.0",
        "Attributes": { "Timestamp": past }
    }]));

    client
        .put_parameter()
        .name("/policies/expired-now")
        .value("v1")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies)
        .send()
        .await
        .unwrap();

    // Lazy deletion fires on the next read.
    let err = client
        .get_parameter()
        .name("/policies/expired-now")
        .send()
        .await
        .expect_err("expected ParameterNotFound for expired param");
    let svc_err = err.into_service_error();
    assert!(
        svc_err.is_parameter_not_found(),
        "expected ParameterNotFound, got: {svc_err:?}"
    );

    // Admin endpoint records the deletion event.
    let body = fetch_policy_events(&server).await;
    let events = body["events"].as_array().unwrap();
    let deletion = events
        .iter()
        .find(|e| e["eventType"] == "Expiration" && e["parameterName"] == "/policies/expired-now")
        .expect("expected Expiration deletion event");
    assert!(deletion["message"]
        .as_str()
        .unwrap()
        .contains("was deleted"));
}

#[tokio::test]
async fn ssm_expiration_in_future_still_readable() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    let future = "9999-01-01T00:00:00.000Z";
    let policies = policies_json(json!([{
        "Type": "Expiration",
        "Version": "1.0",
        "Attributes": { "Timestamp": future }
    }]));

    client
        .put_parameter()
        .name("/policies/future")
        .value("still-here")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies)
        .send()
        .await
        .unwrap();

    let resp = client
        .get_parameter()
        .name("/policies/future")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.parameter().unwrap().value().unwrap(), "still-here");
}

#[tokio::test]
async fn ssm_no_change_notification_fires_after_window() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    // 1 second `After` window: real AWS only accepts Days/Hours, but
    // fakecloud also recognises Minutes/Seconds so E2E tests can wait
    // past the threshold without burning real time. Verifies the lazy
    // tick fires when reads happen after `last_modified + window`.
    let policies = policies_json(json!([{
        "Type": "NoChangeNotification",
        "Version": "1.0",
        "Attributes": { "After": "1", "Unit": "Seconds" }
    }]));

    client
        .put_parameter()
        .name("/policies/inactive")
        .value("v1")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies)
        .send()
        .await
        .unwrap();

    // Wait past the 1s window then read to drive a lazy tick.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    client
        .get_parameter()
        .name("/policies/inactive")
        .send()
        .await
        .unwrap();

    let body = fetch_policy_events(&server).await;
    let events = body["events"].as_array().unwrap();

    let registered = events.iter().any(|e| {
        e["eventType"] == "NoChangeNotificationRegistered"
            && e["parameterName"] == "/policies/inactive"
    });
    assert!(
        registered,
        "expected NoChangeNotificationRegistered event, got {events:?}"
    );

    let fired = events.iter().any(|e| {
        e["eventType"] == "NoChangeNotification" && e["parameterName"] == "/policies/inactive"
    });
    assert!(
        fired,
        "expected NoChangeNotification to fire after window; events={events:?}"
    );
}

#[tokio::test]
async fn ssm_expiration_notification_fires_within_window() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    // Expire 10 seconds from now, with a Before=20 second window so
    // the notification fires immediately on the first read.
    let exp_at = chrono::Utc::now() + chrono::Duration::seconds(10);
    let policies = policies_json(json!([
        {
            "Type": "Expiration",
            "Version": "1.0",
            "Attributes": { "Timestamp": exp_at.to_rfc3339() }
        },
        {
            "Type": "ExpirationNotification",
            "Version": "1.0",
            "Attributes": { "Before": "20", "Unit": "Seconds" }
        }
    ]));

    client
        .put_parameter()
        .name("/policies/expiring-soon")
        .value("v1")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies)
        .send()
        .await
        .unwrap();

    client
        .get_parameter()
        .name("/policies/expiring-soon")
        .send()
        .await
        .unwrap();

    let body = fetch_policy_events(&server).await;
    let events = body["events"].as_array().unwrap();
    let fired = events.iter().any(|e| {
        e["eventType"] == "ExpirationNotification"
            && e["parameterName"] == "/policies/expiring-soon"
    });
    assert!(
        fired,
        "expected ExpirationNotification to fire within window; events={events:?}"
    );
}

#[tokio::test]
async fn ssm_policy_on_standard_tier_rejected() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    let policies = policies_json(json!([{
        "Type": "Expiration",
        "Version": "1.0",
        "Attributes": { "Timestamp": "9999-01-01T00:00:00.000Z" }
    }]));

    let err = client
        .put_parameter()
        .name("/policies/standard-rejected")
        .value("nope")
        .r#type(ParameterType::String)
        // No .tier() => Standard default
        .policies(policies)
        .send()
        .await
        .expect_err("expected Standard-tier policy rejection");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Advanced") || msg.contains("ValidationException"),
        "expected Advanced-tier validation error, got: {msg}"
    );
}

#[tokio::test]
async fn ssm_malformed_policies_rejected() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    // Missing required Attributes.Timestamp -> InvalidPolicyAttributeException.
    let policies = policies_json(json!([{
        "Type": "Expiration",
        "Version": "1.0",
        "Attributes": {}
    }]));

    let err = client
        .put_parameter()
        .name("/policies/malformed")
        .value("nope")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies)
        .send()
        .await
        .expect_err("expected InvalidPolicyAttributeException");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("InvalidPolicyAttribute") || msg.contains("Timestamp"),
        "expected InvalidPolicyAttributeException, got: {msg}"
    );
}

/// Regression: a NoChangeNotification window past chrono's range was accepted
/// and then panicked the policy evaluator that runs on every read, poisoning
/// the account. It must be rejected with InvalidPolicyAttributeException, and a
/// representable but unreachable window must leave reads working.
#[tokio::test]
async fn ssm_out_of_range_policy_window_rejected_and_reads_survive() {
    let server = TestServer::start().await;
    let client = server.ssm_client().await;

    let err = client
        .put_parameter()
        .name("/policies/huge-window")
        .value("v1")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies_json(json!([{
            "Type": "NoChangeNotification",
            "Version": "1.0",
            "Attributes": { "After": "9223372036854775807", "Unit": "Days" }
        }])))
        .send()
        .await
        .expect_err("an out-of-range window must be rejected")
        .into_service_error();
    assert!(
        err.is_invalid_policy_attribute_exception(),
        "expected InvalidPolicyAttributeException, got: {err:?}"
    );

    client
        .put_parameter()
        .name("/policies/far-window")
        .value("v1")
        .r#type(ParameterType::String)
        .tier(ParameterTier::Advanced)
        .policies(policies_json(json!([{
            "Type": "NoChangeNotification",
            "Version": "1.0",
            "Attributes": { "After": "100000000", "Unit": "Days" }
        }])))
        .send()
        .await
        .unwrap();

    // Every read ticks the policy evaluator; it must not panic.
    for _ in 0..2 {
        let got = client
            .get_parameter()
            .name("/policies/far-window")
            .send()
            .await
            .unwrap();
        assert_eq!(got.parameter.unwrap().value.as_deref(), Some("v1"));
    }
    let body = fetch_policy_events(&server).await;
    let events = body["events"].as_array().unwrap();
    assert!(!events
        .iter()
        .any(|e| e["eventType"] == "NoChangeNotification"
            && e["parameterName"] == "/policies/far-window"));
}
