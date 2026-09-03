//! Exact rational time — the anti-float-drift core of ADR-0015 §2.1.
//!
//! OTIO `RationalTime` is (value, rate) with both as f64 in the wire form;
//! python-otio computes value/rate with float division, so non-dyadic rates
//! (NTSC 24000/1001) already lose bits at the wire. The merge MUST NOT stack
//! float error through trim arithmetic: every delta is computed exactly here,
//! and conversion back to the wire form happens once, at the edge, under an
//! explicit lossless-or-refuse policy.
//!
//! Representation: `num`/`den` normalized (den > 0, gcd(|num|,den) = 1) with
//! magnitudes bounded by 2^95, so cross-multiplication for comparison needs at
//! most 190 bits — handled exactly by the 256-bit limb helper below (no
//! float anywhere in a comparison).

use core::cmp::Ordering;

/// Upper bound (exclusive) on `|num|` and `den`: 2^95.
const MAG_BOUND: u128 = 1u128 << 95;

/// An exact rational value. Invariants: `den > 0`, `gcd(|num|, den) = 1`,
/// `|num| < 2^95`, `den < 2^95`. Zero is `0/1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rational {
    pub num: i128,
    pub den: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RationalError {
    /// Zero denominator.
    ZeroDen,
    /// Magnitude at/above the 2^95 bound (sane NLE rates never reach it;
    /// refusing beats silently losing exactness — the C10 honesty policy).
    OutOfRange,
    /// No exact f64 representation exists (non-dyadic rational), or the value
    /// overflows f64 range.
    NotLosslessF64,
}

impl Rational {
    pub const ZERO: Rational = Rational { num: 0, den: 1 };

    /// Exact constructor: normalizes sign (den > 0), reduces by gcd, enforces
    /// the magnitude bound.
    pub fn new(num: i128, den: i128) -> Result<Rational, RationalError> {
        if den == 0 {
            return Err(RationalError::ZeroDen);
        }
        let (mut n, mut d) = (num, den);
        if d < 0 {
            n = -n;
            d = -d;
        }
        let d_u = u128::try_from(d).map_err(|_| RationalError::OutOfRange)?;
        let g = gcd(n.unsigned_abs(), d_u).max(1);
        let n = n / g as i128;
        let d_u = d_u / g;
        if n.unsigned_abs() >= MAG_BOUND || d_u >= MAG_BOUND {
            return Err(RationalError::OutOfRange);
        }
        Ok(Rational { num: n, den: d_u })
    }

    /// value/rate pair (OTIO `RationalTime` semantics, kept exact).
    pub fn from_parts(value: i128, rate: i128) -> Result<Rational, RationalError> {
        Rational::new(value, rate)
    }

    /// Exact addition (errors propagate the magnitude bound — a merge that
    /// cannot stay exact escalates, never wraps: I2).
    pub fn checked_add(self, rhs: Rational) -> Result<Rational, RationalError> {
        let n = self
            .num
            .checked_mul(rhs.den as i128)
            .and_then(|a| a.checked_add(rhs.num.checked_mul(self.den as i128)?))
            .ok_or(RationalError::OutOfRange)?;
        let d = (self.den as i128)
            .checked_mul(rhs.den as i128)
            .ok_or(RationalError::OutOfRange)?;
        Rational::new(n, d)
    }

    /// Exact negation (cannot fail: magnitudes unchanged).
    #[must_use]
    pub fn negated(self) -> Rational {
        Rational {
            num: -self.num,
            den: self.den,
        }
    }

    /// Exact subtraction.
    pub fn checked_sub(self, rhs: Rational) -> Result<Rational, RationalError> {
        self.checked_add(rhs.negated())
    }

    /// Exact multiplication.
    pub fn checked_mul(self, rhs: Rational) -> Result<Rational, RationalError> {
        let n = self
            .num
            .checked_mul(rhs.num)
            .ok_or(RationalError::OutOfRange)?;
        let d = (self.den as i128)
            .checked_mul(rhs.den as i128)
            .ok_or(RationalError::OutOfRange)?;
        Rational::new(n, d)
    }

    /// Exact division (rhs must be non-zero).
    pub fn checked_div(self, rhs: Rational) -> Result<Rational, RationalError> {
        if rhs.num == 0 {
            return Err(RationalError::ZeroDen);
        }
        let n = self
            .num
            .checked_mul(rhs.den as i128)
            .ok_or(RationalError::OutOfRange)?;
        let d = (self.den as i128)
            .checked_mul(rhs.num)
            .ok_or(RationalError::OutOfRange)?;
        Rational::new(n, d)
    }

    /// Exact comparison — no floats anywhere on this path.
    pub fn cmp_exact(self, rhs: Rational) -> Ordering {
        if self.den == rhs.den {
            return self.num.cmp(&rhs.num);
        }
        let sa = self.num < 0;
        let sb = rhs.num < 0;
        let la = self.num.unsigned_abs();
        let lb = rhs.num.unsigned_abs();
        let pa = mul_limbs(la, rhs.den);
        let pb = mul_limbs(lb, self.den);
        let mag = cmp_limbs(&pa, &pb);
        match (sa, sb) {
            (false, false) => mag,
            (true, true) => mag.reverse(),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
        }
    }

    /// Lossless f64 conversion: succeeds iff this rational is exactly equal to
    /// the correctly-rounded `num as f64 / den as f64` (dyadic rationals within
    /// range; subnormals included only when exact — checked by reconstruction).
    pub fn to_f64_lossless(self) -> Result<f64, RationalError> {
        let v = self.num as f64 / self.den as f64;
        if !v.is_finite() {
            return Err(RationalError::NotLosslessF64);
        }
        if f64_to_rational(v)? == self {
            Ok(v)
        } else {
            Err(RationalError::NotLosslessF64)
        }
    }

    /// Best-effort f64 for reports/humans only — never for merge arithmetic.
    pub fn to_f64_approx(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// True iff exactly zero.
    pub fn is_zero(self) -> bool {
        self.num == 0
    }

    /// Convert to a numerator over a positive integer rate, if it divides
    /// exactly (FCPXML `num/den` frames, frame-quantized output).
    pub fn over_rate(self, rate: u128) -> Option<i128> {
        let scaled = self.num.checked_mul(rate as i128)?;
        if scaled % self.den as i128 == 0 {
            Some(scaled / self.den as i128)
        } else {
            None
        }
    }
}

/// f64 → exact Rational via the IEEE-754 mantissa: every finite double IS a
/// dyadic rational (m · 2^e with |m| < 2^53), so this conversion is exact.
pub fn f64_to_rational(v: f64) -> Result<Rational, RationalError> {
    if !v.is_finite() {
        return Err(RationalError::OutOfRange);
    }
    if v == 0.0 {
        return Ok(Rational::ZERO);
    }
    let bits = v.to_bits();
    let sign = if bits >> 63 == 1 { -1i128 } else { 1 };
    let exp_field = ((bits >> 52) & 0x7ff) as i32;
    let mant = i128::from(bits & 0x000f_ffff_ffff_ffff);
    let (mant, exp) = if exp_field == 0 {
        (mant, -1074_i32) // subnormal: no implicit leading 1
    } else {
        (mant + (1 << 52), exp_field - 1075)
    };
    let num = sign * mant;
    if exp >= 0 {
        let factor = 2i128
            .checked_pow(u32::try_from(exp).map_err(|_| RationalError::OutOfRange)?)
            .ok_or(RationalError::OutOfRange)?;
        let num = num.checked_mul(factor).ok_or(RationalError::OutOfRange)?;
        Rational::new(num, 1)
    } else {
        let factor = 2u128
            .checked_pow(u32::try_from(-exp).map_err(|_| RationalError::OutOfRange)?)
            .ok_or(RationalError::OutOfRange)?;
        Rational::new(num, factor as i128)
    }
}

fn gcd(a: u128, b: u128) -> u128 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// u128 × u128 → exact 256-bit product as four little-endian u64 limbs.
///
/// Structure: the four 64×64 partial products are added at their shifted limb
/// positions with a one-limb carry ripple (`add_u128_at`). Two earlier drafts
/// had carry-domain bugs (folding `prod_hi` into the next limb-1 addition;
/// re-adding a limb-2 carry-out into limb 2) — both caught by the u128-boundary
/// known-answer vectors below, which is why those vectors exist.
fn mul_limbs(a: u128, b: u128) -> [u64; 4] {
    let al = a as u64;
    let ah = (a >> 64) as u64;
    let bl = b as u64;
    let bh = (b >> 64) as u64;
    let mut r = [0u64; 4];
    add_u128_at(&mut r, 0, u128::from(al) * u128::from(bl));
    add_u128_at(&mut r, 1, u128::from(al) * u128::from(bh));
    add_u128_at(&mut r, 1, u128::from(ah) * u128::from(bl));
    add_u128_at(&mut r, 2, u128::from(ah) * u128::from(bh));
    r
}

/// Add the 128-bit value `p` shifted by `at·64` bits into the limb array,
/// rippling carries upward (total product < 2^256, so the final ripple is 0).
fn add_u128_at(r: &mut [u64; 4], at: usize, p: u128) {
    let lo = p as u64;
    let hi = (p >> 64) as u64;
    let t = u128::from(r[at]) + u128::from(lo);
    r[at] = t as u64;
    let carry = (t >> 64) as u64; // 0 or 1
    let t2 = u128::from(r[at + 1]) + u128::from(hi) + u128::from(carry);
    r[at + 1] = t2 as u64;
    let carry2 = (t2 >> 64) as u64; // 0 or 1
    if carry2 > 0 {
        let t3 = u128::from(r[at + 2]) + u128::from(carry2);
        debug_assert_eq!(t3 >> 64, 0, "carry escaped 256 bits");
        r[at + 2] = t3 as u64;
    }
}

/// Lexicographic comparison of 4-limb little-endian values (most significant
/// limb first).
fn cmp_limbs(a: &[u64; 4], b: &[u64; 4]) -> Ordering {
    for i in (0..4).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_normalization() {
        assert_eq!(Rational::ZERO.num, 0);
        assert_eq!(Rational::new(24, 24).unwrap(), Rational::new(1, 1).unwrap());
        assert_eq!(Rational::new(2, 4).unwrap(), Rational::new(1, 2).unwrap());
        assert_eq!(Rational::new(-2, 4).unwrap(), Rational::new(-1, 2).unwrap());
        // negative denominator normalizes
        assert_eq!(Rational::new(1, -2).unwrap(), Rational::new(-1, 2).unwrap());
    }

    #[test]
    fn add_sub_exact() {
        let a = Rational::new(1, 3).unwrap();
        let b = Rational::new(1, 6).unwrap();
        assert_eq!(a.checked_add(b).unwrap(), Rational::new(1, 2).unwrap());
        assert_eq!(a.checked_sub(a).unwrap(), Rational::ZERO);
        // 1/10 + 2/10 = 3/10 — the float-world trap, exact here
        assert_eq!(
            Rational::new(1, 10)
                .unwrap()
                .checked_add(Rational::new(2, 10).unwrap())
                .unwrap(),
            Rational::new(3, 10).unwrap()
        );
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn cmp_exact_all_paths() {
        let a = Rational::new(1, 3).unwrap();
        let b = Rational::new(2, 3).unwrap();
        assert_eq!(a.cmp_exact(b), Ordering::Less);
        assert_eq!(b.cmp_exact(a), Ordering::Greater);
        assert_eq!(a.cmp_exact(a), Ordering::Equal);
        // 64-bit+ path: operands near the 2^95 bound
        let big = Rational::new((1 << 94) - 1, 3).unwrap();
        let big2 = Rational::new((1 << 94) - 2, 3).unwrap();
        assert_eq!(big.cmp_exact(big2), Ordering::Greater);
        assert_eq!(big2.cmp_exact(big), Ordering::Less);
        // cross-side magnitudes with different dens (hits the limb path)
        let x = Rational::new((1 << 90) - 1, (1 << 60) + 1).unwrap();
        let y = Rational::new((1 << 90) - 2, (1 << 60) + 3).unwrap();
        let o = x.cmp_exact(y);
        let o2 = y.cmp_exact(x);
        assert_eq!(o.reverse(), o2);
        // signs
        assert_eq!(big.negated().cmp_exact(big2), Ordering::Less);
        assert_eq!(big.cmp_exact(big2.negated()), Ordering::Greater);
        assert_eq!(a.negated().cmp_exact(b.negated()), Ordering::Greater);
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn mul_limbs_known_answers_and_structure() {
        // boundary vectors computed independently (Python bignum, 2026-09-03):
        // (2^128-2)(2^128-3) = 2^256 - 5·2^128 + 6
        let a = u128::MAX - 1;
        let b = u128::MAX - 2;
        assert_eq!(mul_limbs(a, b), [6, 0, u64::MAX - 4, u64::MAX]);
        // (2^128-1)^2 = 2^256 - 2^129 + 1
        assert_eq!(
            mul_limbs(u128::MAX, u128::MAX),
            [1, 0, u64::MAX - 1, u64::MAX]
        );
        // a realistic in-bound pair (operands < 2^95)
        let x: u128 = (1 << 90) - 1;
        let y: u128 = (1 << 60) + 3;
        let want = [0xefff_ffff_ffff_fffdu64, 0x0bff_ffffu64, 0x40_0000u64, 0];
        assert_eq!(mul_limbs(x, y), want);
        // low 128 bits always equal the wrapping product
        for &(a, b) in &[
            (0u128, 0u128),
            (1, 1),
            (0xFFFF_FFFF_FFFF_FFFF, 0xFFFF_FFFF_FFFF_FFFF),
            (1u128 << 94, (1 << 94) + 7),
            (u128::MAX - 1, u128::MAX - 2),
            (12345678901234567890u128, 9876543210987654321u128),
        ] {
            let got = mul_limbs(a, b);
            let lo = u128::from(got[0]) | (u128::from(got[1]) << 64);
            assert_eq!(lo, a.wrapping_mul(b), "low limb mismatch for a={a} b={b}");
            // symmetry
            let rev = mul_limbs(b, a);
            assert_eq!(got, rev, "not symmetric for a={a} b={b}");
            // small operands: high limbs zero and exact u128 product
            if a <= u128::from(u64::MAX) && b <= u128::from(u64::MAX) {
                let hi = u128::from(got[2]) | (u128::from(got[3]) << 64);
                assert_eq!(hi, 0);
                assert_eq!(lo, a * b);
            }
        }
        // b == 1: limbs equal a
        assert_eq!(
            mul_limbs(u128::MAX - 7, 1),
            [
                (u128::MAX - 7) as u64,
                u64::try_from((u128::MAX - 7) >> 64).unwrap(),
                0,
                0
            ]
        );
    }

    #[test]
    #[allow(clippy::float_cmp)] // to_f64_lossless is EXACT by contract — equality is the test
    fn f64_lossless_policy() {
        assert_eq!(Rational::new(1, 2).unwrap().to_f64_lossless().unwrap(), 0.5);
        assert_eq!(
            Rational::new(24, 24).unwrap().to_f64_lossless().unwrap(),
            1.0
        );
        // 1/3 is not representable exactly as f64
        assert!(Rational::new(1, 3).unwrap().to_f64_lossless().is_err());
        // subnormal/tiny/huge dyadics are OUTSIDE the bounded model (2^95) —
        // refused honestly rather than approximated (C10 policy)
        assert!(f64_to_rational(f64::MIN_POSITIVE).is_err());
        assert!(f64_to_rational(1e300).is_err());
        assert!(f64_to_rational(1e-300).is_err());
        // overflow refuses
        assert!(Rational::new(1, 1).unwrap().over_rate(1).is_some());
        assert!(matches!(
            Rational::new(1i128 << 94, 1)
                .and_then(|r| r.checked_add(Rational::new(1i128 << 94, 1).unwrap())),
            Err(RationalError::OutOfRange)
        ));
    }

    #[test]
    #[allow(clippy::float_cmp)] // roundtrip equality of correctly-rounded doubles IS the test
    fn f64_to_rational_is_exact_for_every_dyadic() {
        for v in [
            0.0,
            1.0,
            0.5,
            0.25,
            24.0,
            23.976,
            -12.5,
            0.1,
            1e10,
            2.0f64.powi(-40),
        ] {
            let r = f64_to_rational(v).unwrap();
            assert_eq!(r.to_f64_approx(), v, "roundtrip for {v}");
        }
        // beyond the 2^95 magnitude bound: refused, never approximated
        for v in [1e-300, 1e300, f64::MIN_POSITIVE] {
            assert!(f64_to_rational(v).is_err(), "{v} must refuse");
        }
        let r = f64_to_rational(0.1).unwrap();
        // 0.1 as double is 0x1999999999999A · 2^-56, reduced: 3602879701896397/2^55
        assert_eq!(r.den, 1u128 << 55);
        assert_eq!(r.num, 3602879701896397);
        // reconstruction is normalized: 2/4 halves both
        assert_eq!(Rational::new(2, 4).unwrap().den, 2);
    }

    #[test]
    fn over_rate_quantizes() {
        let r = Rational::new(48, 24).unwrap();
        assert_eq!(r.over_rate(24), Some(48));
        // 1/3 s = 8 frames @24 fps exactly (divides!)
        assert_eq!(Rational::new(1, 3).unwrap().over_rate(24), Some(8));
        // 1/5 s does not quantize to whole frames @24
        assert_eq!(Rational::new(1, 5).unwrap().over_rate(24), None);
        assert_eq!(Rational::new(1, 3).unwrap().over_rate(3), Some(1));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(Rational::new(1, 0), Err(RationalError::ZeroDen)));
        assert!(matches!(
            Rational::new(1 << 100, 1),
            Err(RationalError::OutOfRange)
        ));
        assert!(matches!(
            Rational::new(1, -(1 << 100)),
            Err(RationalError::OutOfRange)
        ));
    }
}
