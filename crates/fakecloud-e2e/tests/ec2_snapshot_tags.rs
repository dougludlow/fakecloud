//! EBS snapshot tagging E2E: tags requested on CopySnapshot / CreateSnapshots
//! (TagSpecifications, CopyTagsFromSource) read back via DescribeSnapshots.
//! Metadata-only, no container runtime needed.

mod helpers;

use aws_sdk_ec2::types::{
    CopyTagsFromSource, InstanceSpecification, ResourceType, Tag, TagSpecification,
};
use helpers::TestServer;

fn snapshot_tag_spec(key: &str, value: &str) -> TagSpecification {
    TagSpecification::builder()
        .resource_type(ResourceType::Snapshot)
        .tags(Tag::builder().key(key).value(value).build())
        .build()
}

async fn snapshot_tags(c: &aws_sdk_ec2::Client, id: &str) -> Vec<(String, String)> {
    let resp = c
        .describe_snapshots()
        .snapshot_ids(id)
        .send()
        .await
        .unwrap();
    let mut tags: Vec<(String, String)> = resp.snapshots()[0]
        .tags()
        .iter()
        .map(|t| {
            (
                t.key().unwrap_or_default().to_string(),
                t.value().unwrap_or_default().to_string(),
            )
        })
        .collect();
    tags.sort();
    tags
}

#[tokio::test]
async fn copy_snapshot_tag_specifications_are_described() {
    let s = TestServer::start().await;
    let c = s.ec2_client().await;
    let vol = c
        .create_volume()
        .availability_zone("us-east-1a")
        .size(8)
        .send()
        .await
        .unwrap();
    let snap = c
        .create_snapshot()
        .volume_id(vol.volume_id().unwrap())
        .send()
        .await
        .unwrap();
    let copy = c
        .copy_snapshot()
        .source_region("us-east-1")
        .source_snapshot_id(snap.snapshot_id().unwrap())
        .tag_specifications(snapshot_tag_spec("env", "copy"))
        .send()
        .await
        .unwrap();
    assert_eq!(copy.tags().len(), 1);
    assert_eq!(
        snapshot_tags(&c, copy.snapshot_id().unwrap()).await,
        vec![("env".to_string(), "copy".to_string())]
    );
}

#[tokio::test]
async fn create_snapshots_copies_volume_tags_and_tag_specifications() {
    let s = TestServer::start().await;
    let c = s.ec2_client().await;
    let run = c
        .run_instances()
        .image_id("ami-12345678")
        .min_count(1)
        .max_count(1)
        .send()
        .await
        .unwrap();
    let instance_id = run.instances()[0].instance_id().unwrap().to_string();
    let az = run.instances()[0]
        .placement()
        .and_then(|p| p.availability_zone())
        .unwrap_or("us-east-1a")
        .to_string();
    let vol = c
        .create_volume()
        .availability_zone(&az)
        .size(8)
        .tag_specifications(
            TagSpecification::builder()
                .resource_type(ResourceType::Volume)
                .tags(Tag::builder().key("team").value("core").build())
                .build(),
        )
        .send()
        .await
        .unwrap();
    c.attach_volume()
        .volume_id(vol.volume_id().unwrap())
        .instance_id(&instance_id)
        .device("/dev/sdf")
        .send()
        .await
        .unwrap();
    let out = c
        .create_snapshots()
        .instance_specification(
            InstanceSpecification::builder()
                .instance_id(&instance_id)
                .build(),
        )
        .copy_tags_from_source(CopyTagsFromSource::Volume)
        .tag_specifications(snapshot_tag_spec("env", "dev"))
        .send()
        .await
        .unwrap();
    let snap = out
        .snapshots()
        .iter()
        .find(|s| s.volume_id() == vol.volume_id())
        .expect("snapshot of the attached volume");
    assert_eq!(
        snapshot_tags(&c, snap.snapshot_id().unwrap()).await,
        vec![
            ("env".to_string(), "dev".to_string()),
            ("team".to_string(), "core".to_string()),
        ]
    );
}
