//! The port pool: what Strom hands out, and what it remembers.
//!
//! [`pool`] is the allocator and holds no state beyond the reservations
//! themselves; [`store`] persists them. They are split because the pool is
//! pure — it takes `now`, a probe callback and the live flow set — and stays
//! testable without a clock, a socket or a filesystem.

pub mod pool;
pub mod store;

pub use pool::{PortPool, PortPoolError, ReservationOutcome, Reserved};
pub use store::PortReservationStore;
