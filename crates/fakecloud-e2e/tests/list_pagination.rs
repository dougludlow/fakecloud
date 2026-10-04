//! Page-size / token paging through the official SDKs for list operations
//! that used to validate the page size and then return everything: ELBv2
//! (PageSize / Marker), CloudWatch (MaxResults / NextToken) and API Gateway v2
//! (MaxResults / NextToken).

mod helpers;

use helpers::TestServer;

#[tokio::test]
async fn elbv2_describe_target_groups_pages_by_marker() {
    let server = TestServer::start().await;
    let c = server.elbv2_client().await;
    for i in 0..7 {
        c.create_target_group()
            .name(format!("tg-page-{i}"))
            .protocol(aws_sdk_elasticloadbalancingv2::types::ProtocolEnum::Http)
            .port(80)
            .target_type(aws_sdk_elasticloadbalancingv2::types::TargetTypeEnum::Ip)
            .send()
            .await
            .unwrap();
    }
    let all: Vec<String> = c
        .describe_target_groups()
        .send()
        .await
        .unwrap()
        .target_groups()
        .iter()
        .map(|t| t.target_group_arn().unwrap().to_string())
        .collect();
    assert_eq!(all.len(), 7);

    let pages: Vec<_> = c
        .describe_target_groups()
        .page_size(5)
        .into_paginator()
        .send()
        .collect::<Vec<_>>()
        .await;
    assert_eq!(pages.len(), 2, "seven groups at five per page");
    let walked: Vec<String> = pages
        .into_iter()
        .flat_map(|p| {
            p.unwrap()
                .target_groups()
                .iter()
                .map(|t| t.target_group_arn().unwrap().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(walked, all);

    let err = c
        .describe_target_groups()
        .marker("not-a-marker")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(err.meta().code(), Some("ValidationError"));
}

#[tokio::test]
async fn cloudwatch_describe_anomaly_detectors_pages_by_token() {
    let server = TestServer::start().await;
    let c = server.cloudwatch_client().await;
    for i in 0..7 {
        c.put_anomaly_detector()
            .single_metric_anomaly_detector(
                aws_sdk_cloudwatch::types::SingleMetricAnomalyDetector::builder()
                    .namespace("Page/Test")
                    .metric_name(format!("m{i}"))
                    .stat("Average")
                    .build(),
            )
            .send()
            .await
            .unwrap();
    }
    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        let page = c
            .describe_anomaly_detectors()
            .max_results(5)
            .set_next_token(token.clone())
            .send()
            .await
            .unwrap();
        assert!(page.anomaly_detectors().len() <= 5);
        seen.extend(page.anomaly_detectors().iter().map(|d| {
            d.single_metric_anomaly_detector()
                .and_then(|s| s.metric_name())
                .unwrap()
                .to_string()
        }));
        token = page.next_token().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    assert_eq!(pages, 2);
    seen.sort();
    assert_eq!(seen, (0..7).map(|i| format!("m{i}")).collect::<Vec<_>>());

    let err = c
        .describe_anomaly_detectors()
        .next_token("not-a-token")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(err.meta().code(), Some("InvalidNextToken"));
}

#[tokio::test]
async fn apigatewayv2_get_apis_pages_by_token() {
    let server = TestServer::start().await;
    let c = server.apigatewayv2_client().await;
    for i in 0..7 {
        c.create_api()
            .name(format!("api-{i}"))
            .protocol_type(aws_sdk_apigatewayv2::types::ProtocolType::Http)
            .send()
            .await
            .unwrap();
    }
    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        let page = c
            .get_apis()
            .max_results("5")
            .set_next_token(token.clone())
            .send()
            .await
            .unwrap();
        assert!(page.items().len() <= 5);
        seen.extend(page.items().iter().map(|a| a.name().unwrap().to_string()));
        token = page.next_token().map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    assert_eq!(pages, 2);
    seen.sort();
    assert_eq!(seen, (0..7).map(|i| format!("api-{i}")).collect::<Vec<_>>());
}
