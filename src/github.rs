//! GitHub data access.
//!
//! Phase 1 only establishes the module boundary. The real client — behind a
//! testable transport trait so tests use mock HTTP rather than the network —
//! along with token discovery, org expansion, bounded concurrency, and
//! retry/backoff, lands in Phase 3.
