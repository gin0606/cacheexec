//! Decisions and data formats, free of side effects. Nothing here touches the
//! filesystem, processes, standard streams, signals or the clock: callers pass
//! in what they observed (bytes, statuses, the current time) and carry out the
//! result. Modules here depend only on each other, never on `crate::shell`.

pub mod delivery;
pub mod execution;
pub mod key;
pub mod policy;
pub mod record;
