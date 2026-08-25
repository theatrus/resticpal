//! The service runtime: canonical status, scheduling, and IPC handling.
//!
//! `ServiceRuntime` is one shared object, but its behavior is split by
//! concern:
//! - [`state`]: shared state types, construction, and low-level accessors
//! - [`scheduler`]: the backup schedule state machine and evaluation cadence
//! - [`lifecycle`]: bookkeeping when backups, retention, and repository
//!   operations finish
//! - [`ipc`]: named-pipe request dispatch, views, and configuration updates
//! - [`enrollment`]: managed-device enrollment and signed-policy application
//! - [`events`]: the control-channel message types
//! - [`helpers`]: small cross-cutting free functions

mod enrollment;
mod events;
mod helpers;
mod ipc;
mod lifecycle;
mod restore;
mod scheduler;
mod state;

pub(crate) use enrollment::ManagedPolicyApplyOutcome;
pub use events::{RestoreQueryOutcome, RestoreQueryRequest, RuntimeEvent, ScheduleAction};
pub(crate) use restore::restore_failure_message;
pub use state::ServiceRuntime;

#[cfg(test)]
mod tests;
