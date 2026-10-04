//! AWS EventBridge Scheduler (`scheduler.amazonaws.com`).
//!
//! Distinct from EventBridge Rules (`events.amazonaws.com`): Scheduler
//! is a standalone service with its own SDK, data model (Schedule,
//! ScheduleGroup, FlexibleTimeWindow, DeadLetterConfig,
//! ActionAfterCompletion), and REST-JSON protocol.

pub mod delivery;
pub mod expr;
pub mod persistence;
pub(crate) mod service;
pub mod simulation;
pub(crate) mod state;
pub mod ticker;

pub use service::SchedulerService;
pub use state::{SchedulerState, SharedSchedulerState};
