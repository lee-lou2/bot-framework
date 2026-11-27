//! Host notification hook for attempts parked on approval, hold, or child delegation.
//!
//! After the ledger parks a run and releases its lease, the front retires only that attempt's
//! session and notifies the host so the CLI process can be torn down without affecting other claims.

use bot_core::id::Attempt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkReason {
    Approval,
    Held,
    Child,
}

pub trait Park: Send + Sync {
    fn parked(&self, attempt: &Attempt, reason: ParkReason);
}

pub struct NoPark;

impl Park for NoPark {
    fn parked(&self, _: &Attempt, _: ParkReason) {}
}
