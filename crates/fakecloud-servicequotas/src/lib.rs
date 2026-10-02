//! Service Quotas (`servicequotas`) awsJson1.1 service for fakecloud.
//!
//! The full 26-operation API from the AWS Smithy model, backed by a catalog
//! of real quota codes and AWS default values (see [`catalog`]). Increase
//! requests are decided immediately and raise the account's applied value;
//! the Organizations quota request template is applied to accounts created
//! in the organization after it is associated. Other services read applied
//! values through [`ServiceQuotasProvider`], so raising a quota changes what
//! they enforce: EC2 checks security groups per network interface and rules
//! per security group against it.

pub mod catalog;
pub mod persistence;
pub mod provider;
pub mod service;
pub mod state;
pub mod validate;

pub use provider::ServiceQuotasProvider;
pub use service::ServiceQuotasService;
pub use state::{
    ServiceQuotasData, SharedServiceQuotasState, SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION,
};
