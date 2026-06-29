//! `actstat` — report the status of the most recent completed GitHub Actions
//! workflow runs across a configured set of repositories.
//!
//! The crate is split into a thin binary (`src/main.rs`) and this library so
//! the logic is unit-testable without spawning a process or touching the
//! network. Every output format renders from one normalized result model
//! (see [`model`]) so GitHub-parsing logic never gets duplicated per format.

pub mod cli;
pub mod config;
pub mod github;
pub mod model;
pub mod render;
