//! The Drive write lease: a Touch ID-authorised backup token gating
//! content-mutating Drive/Sheets/Docs writes ([ADR-0080](../../docs/adrs/adr-0080.md)).
//!
//! Landing in phases per the ADR's own sequencing (issue #1664). This module
//! currently holds only the human-presence primitive
//! ([`authenticate::Authenticator`]) — the ledger, backup, staleness check
//! and `drive lease acquire`/`--lease` wiring follow in later commits.

pub(crate) mod acquire;
pub(crate) mod authenticate;
pub(crate) mod ledger;
