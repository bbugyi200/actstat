//! Configuration model and discovery.
//!
//! Phase 1 only establishes the module boundary; discovery, parsing,
//! validation, and source resolution land in Phase 2. The error type is
//! defined here now so the binary boundary already has something to map.

use thiserror::Error;

/// Errors that can occur while locating, reading, or parsing the config file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No config file was found via any discovery source.
    #[error("no actstat config found (looked in {0})")]
    NotFound(String),

    /// The config file could not be read.
    #[error("failed to read config at {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The config file was found but could not be parsed/validated.
    #[error("invalid config at {path}: {message}")]
    Invalid {
        /// Path of the offending config file.
        path: String,
        /// Human-readable explanation of what was wrong.
        message: String,
    },
}
