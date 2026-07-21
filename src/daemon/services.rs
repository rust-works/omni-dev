//! Built-in daemon services.
//!
//! Each submodule provides one [`DaemonService`](super::service::DaemonService)
//! implementation: [`bridge`] hosts the browser bridge, [`snowflake`] hosts the
//! Snowflake query engine, [`worktrees`] tracks the repos/worktrees open across
//! every VS Code window, and [`sessions`] tracks the running Claude Code sessions
//! across every terminal and window (the real services); [`echo`] is a trivial
//! test service used by the framework's own tests.

pub mod bridge;
pub mod echo;
pub mod github_counters;
pub mod sessions;
pub mod snowflake;
pub mod worktrees;
