//! Optimism-specific constants, types, and helpers.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc as std;

pub mod block;
pub mod cfg;
pub mod context;
pub mod errors;
pub mod host;
pub mod journaled_state;
pub mod result;
pub mod transaction;

pub use block::{Block, BlockGetter};
pub use cfg::{Cfg, CfgGetter, CreateScheme, TransactTo};
pub use context::PerformantContextAccess;
pub use database_interface::{DBErrorMarker, Database, DatabaseGetter};
pub use errors::ErrorGetter;
pub use journaled_state::{Journal, JournalDBError, JournalGetter};
pub use transaction::{Transaction, TransactionGetter, TransactionType};
