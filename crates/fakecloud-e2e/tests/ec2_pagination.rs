//! EC2 `MaxResults` / `NextToken` paging through the official SDK, across
//! operations that page at dispatch (route tables, internet gateways, DHCP
//! options, transit gateways, network ACLs) and one whose handler pages itself
//! (VPCs), driven both by hand and by the SDK's own paginators.

mod helpers;

use helpers::TestServer;

/// Seven of each resource the listings below page over.
async fn seed(c: &aws_sdk_ec2::Client) {
    for i in 0..7 {
        let vpc = c
            .create_vpc()
            .cidr_block(format!("10.{}.0.0/16", 40 + i))
            .send()
            .await
            .unwrap();
        let vpc_id = vpc.vpc().unwrap().vpc_id().unwrap().to_string();
        c.create_route_table().vpc_id(&vpc_id).send().await.unwrap();
        c.create_network_acl().vpc_id(&vpc_id).send().await.unwrap();
        c.create_internet_gateway().send().await.unwrap();
        c.create_transit_gateway().send().await.unwrap();
        c.create_dhcp_options()
            .dhcp_configurations(
                aws_sdk_ec2::types::NewDhcpConfiguration::builder()
                    .key("domain-name")
                    .values(format!("d{i}.example"))
                    .build(),
            )
            .send()
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn describe_route_tables_pages_by_hand() {
    let server = TestServer::start().await;
    let c = server.ec2_client().await;
    seed(&c).await;

    let all: Vec<String> = c
        .describe_route_tables()
        .send()
        .await
        .unwrap()
        .route_tables()
        .iter()
        .map(|r| r.route_table_id().unwrap().to_string())
        .collect();
    assert!(all.len() > 5, "{all:?}");

    let mut walked = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = c
            .describe_route_tables()
            .max_results(5)
            .set_next_token(token.clone())
            .send()
            .await
            .unwrap();
        assert!(page.route_tables().len() <= 5);
        walked.extend(
            page.route_tables()
                .iter()
                .map(|r| r.route_table_id().unwrap().to_string()),
        );
        token = page.next_token().map(str::to_string);
        if token.is_none() {
            break;
        }
        assert!(walked.len() < all.len(), "a token past the last item");
    }
    assert_eq!(walked, all);

    // A token this server never minted is rejected, not taken as page one.
    let err = c
        .describe_route_tables()
        .next_token("not-a-token")
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(err.meta().code(), Some("InvalidParameterValue"));

    // MaxResults outside the model's 5..=100 range is rejected.
    let err = c
        .describe_route_tables()
        .max_results(101)
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    assert_eq!(err.meta().code(), Some("InvalidParameterValue"));
}

#[tokio::test]
async fn sdk_paginators_walk_every_page_losslessly() {
    let server = TestServer::start().await;
    let c = server.ec2_client().await;
    seed(&c).await;

    macro_rules! check {
        ($op:ident, $items:ident, $id:ident) => {{
            let all: Vec<String> = c
                .$op()
                .send()
                .await
                .unwrap()
                .$items()
                .iter()
                .map(|r| r.$id().unwrap().to_string())
                .collect();
            assert!(all.len() > 5, "{}: {all:?}", stringify!($op));
            let pages: Vec<_> = c
                .$op()
                .max_results(5)
                .into_paginator()
                .send()
                .collect::<Vec<_>>()
                .await;
            assert!(
                pages.len() >= 2,
                "{}: {} page(s)",
                stringify!($op),
                pages.len()
            );
            let walked: Vec<String> = pages
                .into_iter()
                .flat_map(|p| {
                    let p = p.unwrap();
                    assert!(p.$items().len() <= 5);
                    p.$items()
                        .iter()
                        .map(|r| r.$id().unwrap().to_string())
                        .collect::<Vec<_>>()
                })
                .collect();
            assert_eq!(walked, all, "{}", stringify!($op));
        }};
    }

    check!(describe_route_tables, route_tables, route_table_id);
    check!(
        describe_internet_gateways,
        internet_gateways,
        internet_gateway_id
    );
    check!(describe_dhcp_options, dhcp_options, dhcp_options_id);
    check!(
        describe_transit_gateways,
        transit_gateways,
        transit_gateway_id
    );
    check!(describe_network_acls, network_acls, network_acl_id);
    // Handler-paged: must not be sliced a second time at dispatch.
    check!(describe_vpcs, vpcs, vpc_id);
}
