//! DynamoDB tables are regional resources: the same table name exists
//! independently in every region of an account, `ListTables` sees only the
//! request region, a table ARN naming another region is not found, and a
//! version 2019.11.21 replica added with `UpdateTable` `ReplicaUpdates` is a
//! live table of its own region whose writes replicate.

mod helpers;

use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, CreateReplicationGroupMemberAction,
    DeleteReplicationGroupMemberAction, KeySchemaElement, KeyType, ReplicationGroupUpdate,
    ScalarAttributeType,
};
use helpers::TestServer;

async fn ddb_in(server: &TestServer, region: &str) -> aws_sdk_dynamodb::Client {
    aws_sdk_dynamodb::Client::new(&server.aws_config_in(region).await)
}

async fn create_table(ddb: &aws_sdk_dynamodb::Client, name: &str) -> String {
    ddb.create_table()
        .table_name(name)
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("pk")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("pk")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .expect("CreateTable")
        .table_description
        .unwrap()
        .table_arn
        .unwrap()
}

async fn put(ddb: &aws_sdk_dynamodb::Client, table: &str, pk: &str, v: &str) {
    ddb.put_item()
        .table_name(table)
        .item("pk", AttributeValue::S(pk.into()))
        .item("v", AttributeValue::S(v.into()))
        .send()
        .await
        .expect("PutItem");
}

async fn get_v(ddb: &aws_sdk_dynamodb::Client, table: &str, pk: &str) -> Option<String> {
    ddb.get_item()
        .table_name(table)
        .key("pk", AttributeValue::S(pk.into()))
        .send()
        .await
        .expect("GetItem")
        .item
        .and_then(|i| i.get("v").and_then(|v| v.as_s().ok().cloned()))
}

async fn list(ddb: &aws_sdk_dynamodb::Client) -> Vec<String> {
    ddb.list_tables()
        .send()
        .await
        .expect("ListTables")
        .table_names
        .unwrap_or_default()
}

#[tokio::test]
async fn same_table_name_coexists_in_two_regions() {
    let server = TestServer::start().await;
    let east = ddb_in(&server, "us-east-1").await;
    let west = ddb_in(&server, "eu-west-1").await;

    let east_arn = create_table(&east, "orders").await;
    let west_arn = create_table(&west, "orders").await;
    assert_eq!(
        east_arn,
        "arn:aws:dynamodb:us-east-1:123456789012:table/orders"
    );
    assert_eq!(
        west_arn,
        "arn:aws:dynamodb:eu-west-1:123456789012:table/orders"
    );
    create_table(&west, "west-only").await;

    put(&east, "orders", "k", "east").await;
    put(&west, "orders", "k", "west").await;
    assert_eq!(get_v(&east, "orders", "k").await.as_deref(), Some("east"));
    assert_eq!(get_v(&west, "orders", "k").await.as_deref(), Some("west"));

    assert_eq!(list(&east).await, vec!["orders"]);
    assert_eq!(list(&west).await, vec!["orders", "west-only"]);

    // DescribeTable by the table's own ARN works in its region.
    let d = west
        .describe_table()
        .table_name(&west_arn)
        .send()
        .await
        .expect("DescribeTable by ARN")
        .table
        .unwrap();
    assert_eq!(d.table_arn.as_deref(), Some(west_arn.as_str()));

    // An ARN naming another region is not found, whether or not the
    // request's region has a same-named table.
    for arn in [
        west_arn.as_str(),
        "arn:aws:dynamodb:eu-west-1:123456789012:table/west-only",
    ] {
        let err = east
            .describe_table()
            .table_name(arn)
            .send()
            .await
            .expect_err("foreign-region ARN must not resolve");
        assert!(
            err.into_service_error().is_resource_not_found_exception(),
            "{arn}"
        );
    }

    // Deleting one region's table leaves the other's.
    east.delete_table()
        .table_name("orders")
        .send()
        .await
        .expect("DeleteTable");
    assert_eq!(get_v(&west, "orders", "k").await.as_deref(), Some("west"));
}

#[tokio::test]
async fn replica_updates_create_a_live_replica_in_another_region() {
    let server = TestServer::start().await;
    let east = ddb_in(&server, "us-east-1").await;
    let west = ddb_in(&server, "eu-west-1").await;

    create_table(&east, "global").await;
    put(&east, "global", "seed", "1").await;
    let desc = east
        .update_table()
        .table_name("global")
        .replica_updates(
            ReplicationGroupUpdate::builder()
                .create(
                    CreateReplicationGroupMemberAction::builder()
                        .region_name("eu-west-1")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("UpdateTable ReplicaUpdates")
        .table_description
        .unwrap();
    assert_eq!(desc.global_table_version.as_deref(), Some("2019.11.21"));
    assert_eq!(
        desc.replicas()
            .iter()
            .filter_map(|r| r.region_name())
            .collect::<Vec<_>>(),
        vec!["eu-west-1"]
    );

    // The replica exists in eu-west-1, with the seeded row.
    let replica = west
        .describe_table()
        .table_name("global")
        .send()
        .await
        .expect("DescribeTable replica")
        .table
        .unwrap();
    assert_eq!(
        replica.table_arn.as_deref(),
        Some("arn:aws:dynamodb:eu-west-1:123456789012:table/global")
    );
    assert_eq!(
        replica
            .replicas()
            .iter()
            .filter_map(|r| r.region_name())
            .collect::<Vec<_>>(),
        vec!["us-east-1"]
    );
    assert_eq!(list(&west).await, vec!["global"]);
    assert_eq!(get_v(&west, "global", "seed").await.as_deref(), Some("1"));

    // Writes replicate both ways.
    put(&west, "global", "w", "from-west").await;
    assert_eq!(
        get_v(&east, "global", "w").await.as_deref(),
        Some("from-west")
    );
    put(&east, "global", "seed", "2").await;
    assert_eq!(get_v(&west, "global", "seed").await.as_deref(), Some("2"));

    // Removing the replica deletes the eu-west-1 table.
    east.update_table()
        .table_name("global")
        .replica_updates(
            ReplicationGroupUpdate::builder()
                .delete(
                    DeleteReplicationGroupMemberAction::builder()
                        .region_name("eu-west-1")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("UpdateTable delete replica");
    let err = west
        .describe_table()
        .table_name("global")
        .send()
        .await
        .expect_err("replica deleted");
    assert!(err.into_service_error().is_resource_not_found_exception());
    assert!(list(&west).await.is_empty());
}
