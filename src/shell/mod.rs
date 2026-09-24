//! Side effects: processes, files, locks, standard streams, signals and the
//! clock. These modules observe the world, hand what they observed to
//! `crate::domain` for decisions, and carry the decisions out.

pub mod cleanup;
pub mod lock;
pub mod replay;
pub mod runner;
pub mod sharing;
pub mod signals;
pub mod store;
pub mod verbose;
