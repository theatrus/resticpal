#![forbid(unsafe_code)]

//! Shared, platform-independent behavior for resticpal.
//!
//! Windows service and UI code should stay thin. Policy resolution, scheduling,
//! and construction of allowlisted restic invocations belong in this crate so
//! they can be exercised without installing a service or touching a repository.

pub mod config;
pub mod management;
pub mod policy;
pub mod restic;
pub mod schedule;
pub mod status;

pub use config::{EffectiveConfig, LocalConfig, RepositoryMode};
pub use policy::{ManagedPolicy, ResolvedConfig, resolve_config};
