//! Entrypoint modules for the StellarSaveContract.
//!
//! Each module owns the function bodies for a single domain.
//! `contract.rs` re-exports them as a thin `#[contractimpl]` façade so
//! that the generated contract spec (XDR / JSON ABI) is unchanged.

pub mod admin;
pub mod contributions;
pub mod group_lifecycle;
pub mod payouts;
pub mod queries;
