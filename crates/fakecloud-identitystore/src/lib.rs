//! AWS IAM Identity Center Identity Store (`identitystore`) awsJson1.1 service
//! for fakecloud.
//!
//! The full 22-operation control plane from the AWS Smithy model:
//! users (Create/Describe/Update/Delete/GetUserId/ListUsers), groups
//! (Create/Describe/Update/Delete/GetGroupId/ListGroups), group
//! memberships (Create/Describe/Delete/GetGroupMembershipId/
//! ListGroupMemberships/ListGroupMembershipsForMember/IsMemberInGroups), and the
//! identity store itself (Describe/List/UpdateIdentityStore). Every
//! operation is backed by real, account-partitioned, persisted state; nested
//! SCIM attribute bags round-trip verbatim.

pub mod persistence;
pub mod service;
pub mod state;

pub use service::{IdentityStoreService, InstanceStoreLookup};
pub use state::{
    IdentityStoreData, SharedIdentityStoreState, IDENTITYSTORE_SNAPSHOT_SCHEMA_VERSION,
};
