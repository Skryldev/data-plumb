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
    /// V6-02: true when the reservation came from `try_reserve_known` —
    /// the full declared size was charged once at admission and `charge`
    /// is a no-op for this lease.
    declared: bool,
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
        // V6-01: acquire the permit ONCE and move it straight into the
        // lease — the old acquire-then-release-then-reacquire dance raced
        // against sibling tasks and panicked in take_permit().
        let permit = self.acquire_permit()?;
        let cur = self.bytes_in_flight.load(Ordering::Relaxed);
        if cur.saturating_add(expected) > self.budget {
            return Err(GateError::BudgetExhausted);
        }
        self.bytes_in_flight.fetch_add(expected, Ordering::Relaxed);
        Ok(GateLease {
            gate: self.clone(),
            reserved: expected,
            // V6-02: a DECLARED size is charged exactly once, here. The
            // lease remembers it so `charge` is a no-op for it — streaming
            // bytes of a known-size upload must NOT be charged on top of
            // the reservation (the old double-charge aborted every
            // declared upload larger than budget/2 mid-stream).
            declared: true,
            _permit: permit,
        })
    }

    /// Admits an unknown-size request (charge-as-you-stream).
    pub fn try_reserve_unknown(self: &Arc<Self>) -> Result<GateLease, GateError> {
        let permit = self.acquire_permit()?;
        Ok(GateLease {
            gate: self.clone(),
            reserved: 0,
            declared: false,
            _permit: permit,
        })
    }

    /// Single, race-free permit acquisition (V6-01). The returned permit is
    /// moved into the lease by the caller; there is no re-acquire step, so
    /// no sibling task can steal the slot in between and no `.expect()`
    /// exists on this path.
    fn acquire_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, GateError> {
        self.permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| GateError::TooManyUploads)
    }
}

impl GateLease {
    /// True when this lease was created by [`UploadGate::try_reserve_known`]
    /// — its budget consumption happened once, at reservation time, and
    /// streamed bytes must not be charged again (V6-02).
    pub fn is_declared(&self) -> bool {
        self.declared
    }

    /// Charges streamed bytes; crossing the budget aborts (the caller maps
    /// this to 413 / RESOURCE_EXHAUSTED mid-stream, which is documented).
    ///
    /// V6-02: charging a DECLARED lease is a no-op — its bytes were
    /// reserved in full by `try_reserve_known`. Only unknown-size leases
    /// charge as they stream. (Over-streaming a declared lease is the
    /// caller's declared-size check, not a budget concern.)
    pub fn charge(&mut self, n: u64) -> Result<(), GateError> {
        if self.declared {
            return Ok(());
        }
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
        // V6-02 note: charge() semantics are exercised on an UNKNOWN lease
        // (declared leases charge exactly once at admission — see
        // h01_known_size_not_double_charged).
        let g = gate(2, 1000);
        {
            let mut lease = g.try_reserve_unknown().unwrap();
            lease.charge(600).unwrap();
            assert_eq!(g.bytes_in_flight(), 600);
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

    // -- V6-02: declared-size leases charge EXACTLY ONCE -----------------

    #[test]
    fn h01_known_size_not_double_charged() {
        // V6-02 regression: a declared upload larger than budget/2 used to
        // abort mid-stream because every streamed byte was charged ON TOP
        // of the reservation (2x consumption). Now charge() is a no-op for
        // declared leases and the upload runs to completion.
        let budget = 2 * 1024 * 1024 * 1024u64;
        let declared = budget / 2 + 64 * 1024 * 1024; // admissible, > budget/2
        let g = gate(64, budget);
        let mut lease = g.try_reserve_known(declared).unwrap();
        assert_eq!(g.bytes_in_flight(), declared);
        let piece = 1024 * 1024u64;
        let mut streamed = 0u64;
        while streamed + piece <= declared {
            lease
                .charge(piece)
                .expect("declared lease must not re-charge");
            streamed += piece;
        }
        assert_eq!(
            g.bytes_in_flight(),
            declared,
            "streaming a declared upload must NOT add to the budget usage"
        );
        drop(lease);
        assert_eq!(g.bytes_in_flight(), 0);
    }

    #[test]
    fn h01_unknown_size_charges_per_chunk() {
        // The unknown-size path still charges as it streams (V6-02 fix must
        // not have broken it).
        let g = gate(4, 1000);
        let mut lease = g.try_reserve_unknown().unwrap();
        lease.charge(600).unwrap();
        assert_eq!(g.bytes_in_flight(), 600);
        assert!(lease.charge(500).is_err(), "budget line still aborts");
        drop(lease);
        assert_eq!(g.bytes_in_flight(), 0);
    }

    #[test]
    fn h01_known_size_releases_on_drop() {
        let g = gate(2, 1000);
        {
            let _lease = g.try_reserve_known(600).unwrap();
        }
        assert_eq!(
            g.bytes_in_flight(),
            0,
            "declared reservation released on drop"
        );
        // permit returned: the slot is reusable.
        assert!(g.try_reserve_known(600).is_ok());
    }

    // -- V6-01: no panic under permit contention --------------------------

    #[test]
    fn h01_gate_no_panic_at_capacity() {
        // 10 rounds x 64 single-shot reservations on a 1-permit gate. The
        // old acquire/release/re-acquire dance panicked when a sibling
        // stole the released permit between the two calls (take_permit's
        // expect). There is no re-acquire anymore; any panic fails.
        for _ in 0..10 {
            let g = gate(1, u64::MAX);
            std::thread::scope(|s| {
                for _ in 0..64 {
                    s.spawn(|| {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _ = g.try_reserve_unknown();
                        }));
                        assert!(r.is_ok(), "reservation panicked under contention");
                    });
                }
            });
        }
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
