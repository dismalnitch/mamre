//! Tests for `mamre::hash::poly_hash`: a polynomial hash modulo the prime
//! p = 2^61 - 1, reading `items` as the digits of a number in base `base`:
//!
//!   hash = c0 * B^(n-1) + c1 * B^(n-2) + ... + c(n-1)   (mod p)
//!
//! The reference below computes that the slow, obvious way, with plain `%`,
//! so it shares no tricks with the real one.

use mamre::hash::poly_hash;
use proptest::prelude::*;

const P: u64 = (1 << 61) - 1;

fn reference(items: &[u64], base: u64) -> u64 {
    let p = P as u128;
    let n = items.len();
    let mut hash = 0u128;
    for (i, &c) in items.iter().enumerate() {
        let mut power = 1u128;
        for _ in 0..(n - 1 - i) {
            power = power * base as u128 % p;
        }
        hash = (hash + c as u128 % p * power) % p;
    }
    hash as u64
}

/// The same hash, combined from two halves the way the tree combines two
/// summaries: shift the left half up by the right half's length, then add.
fn combined(left: &[u64], right: &[u64], base: u64) -> u64 {
    let p = P as u128;
    let mut shift = 1u128;
    for _ in 0..right.len() {
        shift = shift * base as u128 % p;
    }
    let l = poly_hash(left, base).unwrap() as u128;
    let r = poly_hash(right, base).unwrap() as u128;
    ((l * shift % p + r) % p) as u64
}

fn digit() -> impl Strategy<Value = u64> {
    // Small digits, and digits right up against p, where overflow lives.
    prop_oneof![0u64..1000, (P - 1000)..P]
}

fn base() -> impl Strategy<Value = u64> {
    prop_oneof![2u64..1000, (P - 1000)..P]
}

#[test]
fn reads_digits_like_a_decimal_number() {
    assert_eq!(poly_hash(&[1, 2, 3, 4], 10).unwrap(), 1234);
}

#[test]
fn a_single_digit_is_itself() {
    assert_eq!(poly_hash(&[42], 7).unwrap(), 42);
}

#[test]
fn nothing_hashes_to_zero() {
    assert_eq!(poly_hash(&[], 31).unwrap(), 0);
}

#[test]
fn wraps_at_the_prime() {
    // B = 2^60, so [2, 0] is 2^61, which is 1 more than p.
    assert_eq!(poly_hash(&[2, 0], 1 << 60).unwrap(), 1);
}

#[test]
fn the_largest_values_dont_overflow() {
    let big = [P - 1; 64];
    assert_eq!(poly_hash(&big, P - 1).unwrap(), reference(&big, P - 1));
}

#[test]
fn a_base_at_or_past_the_prime_is_an_error() {
    assert!(poly_hash(&[1, 2, 3], P).is_err());
    assert!(poly_hash(&[1, 2, 3], u64::MAX).is_err());
}

#[test]
fn order_matters() {
    assert_ne!(poly_hash(&[1, 2], 31).unwrap(), poly_hash(&[2, 1], 31).unwrap());
}

/// Known, and why the tree adds 1 to every digit before hashing: a zero at
/// the front is invisible, so [0, 5] and [5] collide.
#[test]
fn a_leading_zero_vanishes() {
    assert_eq!(poly_hash(&[0, 5], 31).unwrap(), poly_hash(&[5], 31).unwrap());
}

proptest! {
    #[test]
    fn matches_the_reference(items in prop::collection::vec(digit(), 0..200), base in base()) {
        prop_assert_eq!(poly_hash(&items, base).unwrap(), reference(&items, base));
    }

    #[test]
    fn always_lands_below_the_prime(items in prop::collection::vec(digit(), 0..200), base in base()) {
        prop_assert!(poly_hash(&items, base).unwrap() < P);
    }

    /// The property the tree depends on: hashing a sequence whole gives the
    /// same result as hashing it in two pieces and combining them, wherever
    /// it's split.
    #[test]
    fn splitting_anywhere_and_combining_matches(
        items in prop::collection::vec(digit(), 0..200),
        split in any::<prop::sample::Index>(),
        base in base(),
    ) {
        let at = split.index(items.len() + 1);
        let (left, right) = items.split_at(at);
        prop_assert_eq!(combined(left, right, base), poly_hash(&items, base).unwrap());
    }

    /// Different sequences almost never collide. With p near 2^61, a false
    /// match here would mean a bug, not bad luck.
    #[test]
    fn different_sequences_differ(
        a in prop::collection::vec(1u64..1000, 1..50),
        b in prop::collection::vec(1u64..1000, 1..50),
        base in (P - 1_000_000)..P,
    ) {
        prop_assume!(a != b);
        prop_assert_ne!(poly_hash(&a, base).unwrap(), poly_hash(&b, base).unwrap());
    }
}
