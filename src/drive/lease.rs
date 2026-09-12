//! The Drive write lease: a Touch ID-authorised backup token gating
//! content-mutating Drive/Sheets/Docs writes ([ADR-0080](../../docs/adrs/adr-0080.md)).
//!
//! Landed in phases per the ADR's own sequencing (issue #1664):
//! [`authenticate::Authenticator`] is the human-presence primitive;
//! [`ledger`] is the lease ledger `drive lease acquire`
//! ([`acquire`]) reads and writes; [`check`] is the shared "does this
//! presented `--lease` token authorise this write" gate every
//! content-mutating engine (`drive edit`, every Sheets/Docs write verb)
//! calls. `drive lease restore` (ADR-0080 §4) is the one piece not yet
//! landed.

pub(crate) mod acquire;
pub(crate) mod authenticate;
pub(crate) mod check;
pub(crate) mod ledger;
