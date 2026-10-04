//! Introspection endpoints for EventBridge Scheduler.

mod helpers;

use aws_sdk_scheduler::types::{FlexibleTimeWindow, FlexibleTimeWindowMode, Target};
use aws_sdk_sqs::types::QueueAttributeName;
use helpers::TestServer;

fn off_window() -> FlexibleTimeWindow {
    FlexibleTimeWindow::builder()
        .mode(FlexibleTimeWindowMode::Off)
        .build()
        .unwrap()
}

#[tokio::test]
async fn list_schedules_endpoint_returns_created_schedules() {
    let server = TestServer::start().await;
    let sched = server.scheduler_client().await;

    sched
        .create_schedule()
        .name("intro-1")
        .schedule_expression("rate(1 hour)")
        .flexible_time_window(off_window())
        .target(
            Target::builder()
                .arn("arn:aws:sqs:us-east-1:000000000000:q")
                .role_arn("arn:aws:iam::000000000000:role/s")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let resp: serde_json::Value = reqwest::get(format!(
        "{}/_fakecloud/scheduler/schedules",
        server.endpoint()
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let arr = resp["schedules"].as_array().expect("schedules array");
    assert!(arr
        .iter()
        .any(|s| s["name"] == "intro-1" && s["groupName"] == "default"));
}

#[tokio::test]
async fn fire_schedule_endpoint_triggers_sqs_delivery() {
    let server = TestServer::start().await;
    let sched = server.scheduler_client().await;
    let sqs = server.sqs_client().await;

    let q_url = sqs
        .create_queue()
        .queue_name("fire-intro")
        .send()
        .await
        .unwrap()
        .queue_url
        .unwrap();
    let q_arn = sqs
        .get_queue_attributes()
        .queue_url(&q_url)
        .attribute_names(QueueAttributeName::QueueArn)
        .send()
        .await
        .unwrap()
        .attributes
        .unwrap()
        .get(&QueueAttributeName::QueueArn)
        .unwrap()
        .clone();

    // Use a schedule that will NOT naturally fire within the test (20-year rate)
    // so we can isolate the introspection fire.
    sched
        .create_schedule()
        .name("manual-fire")
        .schedule_expression("rate(365 days)")
        .flexible_time_window(off_window())
        .target(
            Target::builder()
                .arn(q_arn)
                .role_arn("arn:aws:iam::000000000000:role/s")
                .input("{\"forced\":true}")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    // Drain any first-bootstrap auto-fire from the queue. Long-polls
    // up to 1s per round so the bootstrap message has time to land,
    // then loops until the queue stays empty for one round.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let resp = sqs
            .receive_message()
            .queue_url(&q_url)
            .max_number_of_messages(10)
            .wait_time_seconds(1)
            .send()
            .await
            .unwrap();
        if resp.messages().is_empty() || std::time::Instant::now() >= deadline {
            break;
        }
    }

    // Now drive the introspection endpoint.
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/_fakecloud/scheduler/fire/default/manual-fire",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["scheduleArn"]
        .as_str()
        .unwrap()
        .contains("schedule/default/manual-fire"));

    let msgs = sqs
        .receive_message()
        .queue_url(&q_url)
        .wait_time_seconds(3)
        .send()
        .await
        .unwrap();
    let ms = msgs.messages.unwrap_or_default();
    assert!(!ms.is_empty(), "fire endpoint should have delivered");
    assert_eq!(ms[0].body.as_deref(), Some("{\"forced\":true}"));
}

#[tokio::test]
async fn fire_schedule_endpoint_returns_404_for_missing() {
    let server = TestServer::start().await;
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/_fakecloud/scheduler/fire/default/does-not-exist",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Schedules are regional: the listing names each schedule's region, and
/// fire picks the schedule of one account and region.
#[tokio::test]
async fn introspection_is_region_aware() {
    let server = TestServer::start().await;
    let sched = aws_sdk_scheduler::Client::new(&server.aws_config_in("eu-west-1").await);
    sched
        .create_schedule()
        .name("west-only")
        .schedule_expression("rate(7300 days)")
        .flexible_time_window(off_window())
        .target(
            Target::builder()
                .arn("arn:aws:sqs:eu-west-1:000000000000:q")
                .role_arn("arn:aws:iam::000000000000:role/s")
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let sdk = fakecloud_sdk::FakeCloud::new(server.endpoint());
    let listing = sdk.scheduler().get_schedules().await.unwrap();
    let row = listing
        .schedules
        .iter()
        .find(|s| s.name == "west-only")
        .expect("listed");
    assert_eq!(row.region, "eu-west-1");

    // The server's default region has no such schedule.
    assert!(sdk
        .scheduler()
        .fire_schedule("default", "west-only")
        .await
        .is_err());
    let fired = sdk
        .scheduler()
        .fire_schedule_in(
            "default",
            "west-only",
            Some(&row.account_id),
            Some("eu-west-1"),
        )
        .await
        .unwrap();
    assert_eq!(fired.schedule_arn, row.arn);
}
