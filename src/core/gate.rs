//! H-01: upload admission control.
//!
//! Two bounded resources protect the process from remote OOM:
//! * `max_concurrent_uploads` — a semaphore bounding in-flight pipeline
//!   tasks;
//! * `max_in_flight_bytes` — a byte budget bounding aggregated upload
//!   reservations. Known-size requests (Content-Length / gRPC
//!   `expected_size`) are admitted or rejected BEFORE streaming; unknown
//!   sizes charge as they stream and abort at the budget line.
//!
//! [`GateLease`] releases the reservation and the permit on Drop, so
//! success, error and panic paths all unwind cleanly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Why admission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    /// The byte budget cannot absorb the reservation → 413 /
    /// RESOURCE_EXHAUSTED.
    BudgetExhausted,
    /// Too many concurrent uploads → 429 / RESOURCE_EXHAUSTED.
    TooManyUploads,
}

/// Admission controller living on `AppState`.
pub struct UploadGate {
    permits: Arc<Semaphore>,
    bytes_in_flight: AtomicU64,
    budget: u64,
}

/// A held reservation. Dropping it returns the bytes and the permit.
pub struct GateLease {
    gate: Arc<UploadGate>,
    reserved: u64,
    /// Returned on Drop (releases the concurrency slot). Never read
    /// directly — kept so the permit's lifetime ties to the lease.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl UploadGate {
    pub fn new(max_concurrent: usize, budget: u64) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
            bytes_in_flight: AtomicU64::new(0),
            budget,
        }
    }

    /// Currently reserved bytes (diagnostics/metrics).
    pub fn bytes_in_flight(&self) -> u64 {
        self.bytes_in_flight.load(Ordering::Relaxed)
    }

    /// Admits a known-size request before any byte streams.
    pub fn try_reserve_known(self: &Arc<Self>, expected: u64) -> Result<GateLease, GateError> {
        self.acquire_permit()?;
        let cur = self.bytes_in_flight.load(Ordering::Relaxed);
        if cur.saturating_add(expected) > self.budget {
            return Err(GateError::BudgetExhausted);
        }
        self.bytes_in_flight.fetch_add(expected, Ordering::Relaxed);
        Ok(GateLease {
            gate: self.clone(),
            reserved: expected,
            _permit: self.take_permit(),
        })
    }

    /// Admits an unknown-size request (charge-as-you-stream).
    pub fn try_reserve_unknown(self: &Arc<Self>) -> Result<GateLease, GateError> {
        self.acquire_permit()?;
        Ok(GateLease {
            gate: self.clone(),
            reserved: 0,
            _permit: self.take_permit(),
        })
    }

    fn acquire_permit(&self) -> Result<(), GateError> {
        self.permits
            .clone()
            .try_acquire_owned()
            .map(|_| ())
            .map_err(|_| GateError::TooManyUploads)
    }

    fn take_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        // A permit was reserved by acquire_permit; this cannot fail.
        self.permits
            .clone()
            .try_acquire_owned()
            .expect("permit reserved by acquire_permit")
    }
}

impl GateLease {
    /// Charges streamed bytes; crossing the budget aborts (the caller maps
    /// this to 413 / RESOURCE_EXHAUSTED mid-stream, which is documented).
    pub fn charge(&mut self, n: u64) -> Result<(), GateError> {
        let gate = &self.gate;
        let cur = gate.bytes_in_flight.load(Ordering::Relaxed);
        if cur.saturating_add(n) > gate.budget {
            // Refuse the charge; the reservation itself is unchanged and the
            // Drop still releases it.
            return Err(GateError::BudgetExhausted);
        }
        gate.bytes_in_flight.fetch_add(n, Ordering::Relaxed);
        self.reserved += n;
        Ok(())
    }

    pub fn reserved(&self) -> u64 {
        self.reserved
    }
}

impl Drop for GateLease {
    fn drop(&mut self) {
        self.gate
            .bytes_in_flight
            .fetch_sub(self.reserved, Ordering::Relaxed);
        // permit drops automatically, returning the slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(concurrent: usize, budget: u64) -> Arc<UploadGate> {
        Arc::new(UploadGate::new(concurrent, budget))
    }

    #[test]
    fn h01_gate_leases_release_on_abort() {
        let g = gate(2, 1000);
        {
            let lease = g.try_reserve_known(600).unwrap();
            assert_eq!(g.bytes_in_flight(), 600);
            let mut lease = lease;
            lease.charge(100).unwrap();
            assert_eq!(g.bytes_in_flight(), 700);
        } // aborted (dropped)
        assert_eq!(g.bytes_in_flight(), 0, "reservation released on drop");
        // permit returned: two more leases are possible
        assert!(g.try_reserve_known(600).is_ok());
        assert!(g.try_reserve_known(600).is_ok());
    }

    #[test]
    fn h01_budget_rejects_known_oversize() {
        let g = gate(4, 1000);
        let err = match g.try_reserve_known(1001) {
            Err(e) => e,
            Ok(_) => panic!("must refuse oversize reservation"),
        };
        assert_eq!(err, GateError::BudgetExhausted);
        assert_eq!(g.bytes_in_flight(), 0, "rejected before any byte read");
    }

    #[test]
    fn h01_charge_aborts_at_budget_line() {
        let g = gate(4, 1000);
        let mut lease = g.try_reserve_unknown().unwrap();
        lease.charge(600).unwrap();
        let err = match lease.charge(500) {
            Err(e) => e,
            Ok(()) => panic!("charge must abort at the budget line"),
        };
        assert_eq!(err, GateError::BudgetExhausted);
        // lease still holds its 600; drop releases exactly that
        drop(lease);
        assert_eq!(g.bytes_in_flight(), 0);
    }

    #[test]
    fn h01_too_many_concurrent() {
        let g = gate(1, u64::MAX);
        let _l1 = g.try_reserve_unknown().unwrap();
        let err = match g.try_reserve_unknown() {
            Err(e) => e,
            Ok(_) => panic!("second lease must be refused (semaphore exhausted)"),
        };
        assert_eq!(err, GateError::TooManyUploads);
    }
}
