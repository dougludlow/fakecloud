//! Service Quotas (`servicequotas`) awsJson1.1 service for fakecloud.
//!
//! The full 26-operation API from the AWS Smithy model, backed by a catalog
//! of real quota codes and AWS default values (see [`catalog`]). Increase
//! requests are decided immediately (or held `PENDING` for the introspection
//! API to decide, see [`settings::RequestApproval`]) and raise the account's
//! applied value; the Organizations quota request template is applied to
//! accounts created in the organization after it is associated. Other
//! services read enforced limits through [`ServiceQuotasProvider`], so raising
//! a quota changes what they accept. Enforcement is opt-in per quota or
//! globally (see [`settings`]).

pub mod catalog;
pub mod persistence;
pub mod provider;
pub mod service;
pub mod settings;
pub mod state;
pub mod validate;

pub use provider::ServiceQuotasProvider;
pub use service::ServiceQuotasService;
pub use settings::{QuotaSettings, RequestApproval, SharedQuotaSettings};
pub use state::{
    ServiceQuotasData, SharedServiceQuotasState, SERVICEQUOTAS_SNAPSHOT_SCHEMA_VERSION,
};
