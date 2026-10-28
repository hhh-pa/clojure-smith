//! Static work budget for generated programs.
//!
//! Every function pays for what it generates out of a fixed per-function
//! budget: leaves cost a unit per node, calls cost the callee's recorded
//! body cost (times `2^fuel` for recursive callees), and loops charge
//! trips times body cost. Calls that do not fit are not offered; fuel and
//! trip counts shrink to fit instead of overflowing. The entry point gets
//! the sum of body costs plus a fixed slack for fuel above zero, so total
//! program work is bounded by construction and slow-tail timeouts cannot
//! occur at any option setting.
//!
//! Units are abstract form-evaluation steps, which over-approximate real
//! work: all branches of a conditional are charged although only one runs.
//! Literals are charged in printed bytes and loop trips shrink against
//! per-trip cost, so every runtime value's construction is transitively
//! byte-bounded; superlinear operators (`sort`, `join`, ...) pay a
//! [`DATA_OP_PREMIUM`], and accumulator growth (geometric, linear, and
//! nested) is trip-capped. Residual slow tails at extreme option maxima
//! surface as drive errors via the wall-clock timeouts, never as hangs.
//! Calibration is empirical — see the drive sweeps.

/// Work units available to one generated function.
pub const WORK_BUDGET_PER_FUNC: u64 = 8_000;

/// Extra units available to the entry point beyond the sum of callee bodies,
/// spent on fuel above zero for recursive calls.
pub const WORK_SLACK_ENTRY: u64 = 32_768;

/// `2^fuel`, saturating: fuel at 64 or above prices a call as unaffordable
/// rather than overflowing.
pub fn fuel_factor(fuel: u64) -> u64 {
    if fuel >= 64 { u64::MAX } else { 1u64 << fuel }
}

/// Price of a call to a function with body cost `body`: the body once for a
/// plain call, times `2^fuel` when the callee is recursive and entered with
/// `fuel`. Saturates instead of overflowing (fail closed: an enormous price
/// simply refuses the call).
pub fn call_charge(body: u64, recursive: bool, fuel: u64) -> u64 {
    if recursive {
        body.saturating_mul(fuel_factor(fuel))
    } else {
        body
    }
}

/// Largest fuel `k` such that a recursive call with body cost `body` fits in
/// `remaining` units, or `None` when even fuel zero (one base evaluation)
/// does not fit. The caller shrinks its drawn fuel to this maximum.
pub fn max_fuel_for(body: u64, remaining: u64) -> Option<u64> {
    if body > remaining {
        return None;
    }
    let mut k = 0u64;
    while k < 62 && call_charge(body, true, k + 1) <= remaining {
        k += 1;
    }
    Some(k)
}

/// Trip count for a loop/`dotimes` whose body costs `per_trip` per
/// iteration: the drawn count shrunk to fit `remaining`. Always valid —
/// zero trips just yields the base value.
pub fn shrink_trips(drawn: i64, per_trip: u64, remaining: u64) -> i64 {
    let per_trip = per_trip.max(1);
    let afford = (remaining / per_trip).min(i64::MAX as u64) as i64;
    drawn.min(afford).max(0)
}

/// Trip cap for a `loop` whose update mentions the accumulator `refs`
/// times: each trip can grow the accumulator by up to `refs`x (e.g.
/// `(vec (interleave acc acc))` doubles it), so trips are capped to keep
/// `refs^trips` small. Single mentions are handled by
/// [`linear_growth_cap`] instead (`i64::MAX` here leaves that bound in
/// charge).
pub fn acc_growth_cap(refs: usize) -> i64 {
    if refs <= 1 {
        i64::MAX
    } else {
        (32 >> refs.min(5)).max(2)
    }
}

/// Trip cap for linear (`refs <= 1`) accumulation: each trip can add at
/// most the update's own static bytes, so trips are capped to keep
/// `trips * update_bytes` within `remaining`. Complements
/// [`acc_growth_cap`], which covers geometric (`refs >= 2`) growth.
pub fn linear_growth_cap(update_bytes: u64, remaining: u64) -> i64 {
    (remaining / update_bytes.max(1)).min(i64::MAX as u64) as i64
}

/// Extra charge when emitting a data-size-sensitive operator (`sort`,
/// `distinct`, `interleave`, `concat`, `str/join`). Node charges cannot
/// see input bytes, so without a premium a tiny body sorting megabytes
/// would draw hundreds of trips; the premium shrinks those draws.
/// Linear small-constant scans (`count`, `compare`, `includes?`,
/// `subvec`, `take`) carry no premium: their inputs are byte-bounded
/// elsewhere (literals, ranges, capped loops), keeping them sub-ms.
pub const DATA_OP_PREMIUM: u64 = 256;

/// Trip cap for a growable loop whose update contains a nested
/// value-producing loop (`loop`/`for`). Static per-trip charges cannot
/// see the nested loop's runtime output size, so nesting multiplies
/// production opaquely; few trips keep the product small. (Deeper
/// compositions remain bounded by the wall-clock generation and runner
/// timeouts rather than statically.)
pub const NESTED_LOOP_TRIPS: i64 = 4;

/// Per-collection take bound for `doseq`/`for` over `n_bindings` collections
/// (cartesian trips) whose body costs `per_trip` per trip. Zero is valid
/// (empty trips); the result always fits in an `i64` literal.
pub fn take_k(n_bindings: usize, per_trip: u64, remaining: u64) -> i64 {
    debug_assert!((1..=2).contains(&n_bindings));
    let per_trip = per_trip.max(1);
    let trips = remaining / per_trip;
    let k = if n_bindings <= 1 {
        trips.min(i64::MAX as u64)
    } else {
        trips.isqrt().min(i64::MAX as u64)
    };
    k as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuel_factor_saturates() {
        assert_eq!(fuel_factor(0), 1);
        assert_eq!(fuel_factor(8), 256);
        assert_eq!(fuel_factor(63), 1u64 << 63);
        assert_eq!(fuel_factor(64), u64::MAX);
        assert_eq!(fuel_factor(1_000_000), u64::MAX);
    }

    #[test]
    fn call_charge_scales_recursive() {
        assert_eq!(call_charge(500, false, 8), 500);
        assert_eq!(call_charge(500, true, 0), 500);
        assert_eq!(call_charge(500, true, 3), 4_000);
        assert_eq!(call_charge(u64::MAX, true, 1), u64::MAX);
    }

    #[test]
    fn max_fuel_fits() {
        assert_eq!(max_fuel_for(500, 499), None);
        assert_eq!(max_fuel_for(500, 500), Some(0));
        assert_eq!(max_fuel_for(500, 4_000), Some(3));
        assert_eq!(max_fuel_for(500, 7_999), Some(3));
        assert_eq!(max_fuel_for(500, 8_000), Some(4));
        // Even a free body cannot take absurd fuel.
        assert_eq!(max_fuel_for(1, u64::MAX), Some(62));
    }

    #[test]
    fn trips_shrink_to_fit() {
        assert_eq!(shrink_trips(30, 100, 1_000), 10);
        assert_eq!(shrink_trips(5, 100, 1_000), 5);
        assert_eq!(shrink_trips(30, 100, 50), 0);
        assert_eq!(shrink_trips(30, 0, 1_000), 30);
        assert_eq!(shrink_trips(-4, 100, 1_000), 0);
    }

    #[test]
    fn linear_cap_bounds_addends() {
        assert_eq!(linear_growth_cap(100, 8000), 80);
        assert_eq!(linear_growth_cap(8000, 8000), 1);
        assert_eq!(linear_growth_cap(1_000_000, 8000), 0);
        assert_eq!(linear_growth_cap(0, 8000), 8000);
        assert_eq!(linear_growth_cap(100, 0), 0);
        assert_eq!(linear_growth_cap(1, u64::MAX), i64::MAX);
    }

    #[test]
    fn growth_cap_bounds_doubling() {
        assert_eq!(acc_growth_cap(0), i64::MAX);
        assert_eq!(acc_growth_cap(1), i64::MAX);
        assert_eq!(acc_growth_cap(2), 8); // 2^8 = 256x
        assert_eq!(acc_growth_cap(3), 4); // 3^4 = 81x
        assert_eq!(acc_growth_cap(4), 2);
        assert_eq!(acc_growth_cap(5), 2);
        assert_eq!(acc_growth_cap(100), 2);
    }

    #[test]
    fn take_k_fits_cartesian() {
        assert_eq!(take_k(1, 100, 1_000), 10);
        assert_eq!(take_k(2, 100, 1_000), 3); // 3^2 * 100 <= 1000 < 4^2 * 100
        assert_eq!(take_k(2, 100, 50), 0);
        assert_eq!(take_k(1, 0, 500), 500);
        assert_eq!(take_k(1, 1, u64::MAX), i64::MAX);
    }
}
