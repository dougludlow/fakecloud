//! AWS EKS (`eks`) implementation for FakeCloud.

pub mod addon_pod_identity;
pub mod cluster_sg;
pub(crate) mod eks_helpers;
pub mod nodegroup_asg;
pub mod persistence;
pub(crate) mod service;
pub(crate) mod state;

pub use cluster_sg::{
    create_cluster_security_group, delete_cluster_security_group, restore_cluster_security_groups,
    ClusterNetwork, CLUSTER_SECURITY_GROUP_DESCRIPTION,
};
pub use service::{EksService, EKS_ACTIONS};
pub use state::{EksSnapshot, EksState, SharedEksState, EKS_SNAPSHOT_SCHEMA_VERSION};
// Re-exported for the CloudFormation resource_provisioner; the `state` module
// itself is pub(crate).
pub use state::{
    access_entry_arn, addon_arn, cluster_arn, fargate_profile_arn, identity_provider_config_arn,
    nodegroup_arn, pod_identity_association_arn, AccessEntry, Addon, Cluster, FargateProfile,
    IdentityProviderConfig, Nodegroup, PodIdentityAssociation, TagMap, DEFAULT_K8S_VERSION,
};
