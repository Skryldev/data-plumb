//! Failpoint harness (crash-consistency.md): named abort points, compiled
//! to no-ops unless the `failpoints` cargo feature is on. Armed via env:
//! `FAILPOINT="name1,name2#2"` (the `#n` form fires only on the n-th hit
//! of that point).
//!
//! Production builds (without the feature) carry zero cost — every call
//! site compiles to nothing. Tests run with `--features failpoints`.

/// Executes a named failpoint: with the `failpoints` feature and the point
/// armed in `FAILPOINT`, panics with `failpoint:<name>` (a crash the
/// harness/xtask treats as a kill at that point). Without the feature,
/// compiled out entirely.
#[cfg(feature = "failpoints")]
pub fn failpoint(name: &str) {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    static ARMED: OnceLock<Vec<(String, Option<usize>)>> = OnceLock::new();
    static COUNTS: OnceLock<Mutex<HashMap<String, AtomicUsize>>> = OnceLock::new();

    let armed = ARMED.get_or_init(|| {
        std::env::var("FAILPOINT")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|spec| match spec.split_once('#') {
                Some((n, k)) => (n.to_string(), k.parse::<usize>().ok()),
                None => (spec.to_string(), None),
            })
            .collect::<Vec<_>>()
    });

    for (armed_name, nth) in armed {
        if armed_name != name {
            continue;
        }
        let counts = COUNTS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut map = counts.lock().unwrap_or_else(|p| p.into_inner());
        let counter = map
            .entry(name.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
        let hit = counter.fetch_add(1, Ordering::Relaxed) + 1;
        match nth {
            Some(n) if hit != *n => continue,
            _ => panic!("failpoint:{name}"),
        }
    }
}

/// No-op compiled form (production builds).
#[cfg(not(feature = "failpoints"))]
pub fn failpoint(_name: &str) {}

/// Expands to nothing unless the `failpoints` feature is on — call at any
/// crash point named in docs/design/crash-consistency.md.
#[macro_export]
macro_rules! failpoint {
    ($name:expr) => {
        $crate::storage::failpoint::failpoint($name)
    };
}

#[cfg(all(test, feature = "failpoints"))]
mod tests {
    #[test]
    fn failpoint_fires_when_armed() {
        // SAFETY: tests run serially for env mutation; this test owns FAILPOINT.
        unsafe { std::env::set_var("FAILPOINT", "probe-point") };
        let result = std::panic::catch_unwind(|| crate::failpoint!("probe-point"));
        assert!(result.is_err(), "armed point must fire");
        let result = std::panic::catch_unwind(|| crate::failpoint!("not-armed"));
        assert!(result.is_ok(), "unarmed point must not fire");
        unsafe { std::env::remove_var("FAILPOINT") };
    }

    #[test]
    fn nth_form_fires_only_on_nth_hit() {
        unsafe { std::env::set_var("FAILPOINT", "nth-point#2") };
        let r1 = std::panic::catch_unwind(|| crate::failpoint!("nth-point"));
        assert!(r1.is_ok(), "hit 1 must not fire #2");
        let r2 = std::panic::catch_unwind(|| crate::failpoint!("nth-point"));
        assert!(r2.is_err(), "hit 2 must fire #2");
        unsafe { std::env::remove_var("FAILPOINT") };
    }
}
