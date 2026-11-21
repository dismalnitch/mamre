use core::result::Result;
use core::result::Result::{Err, Ok};
pub fn poly_hash(items: &[u64], base: u64) -> Result<u64, u64> {
    if base >= P {
        return Err(0);
    }

    let mut hash: u64 = 0;

    for &c in items {
        hash = add(mul(hash, base), c);
    }
    Ok(hash)
}

const P: u64 = (1 << 61) - 1;
fn mul(a: u64, b: u64) -> u64 {
    let x = a as u128 * b as u128;
    let r = (x as u64 & P) + (x >> 61) as u64;
    if r >= P { r - P } else { r }
}

fn add(a: u64, b: u64) -> u64 {
    let r = a + b;
    if r >= P { r - P } else { r }
}

pub fn pow(mut b: u64, mut k: u64) -> u64 {
    let mut r = 1;
    while k > 0 {
        if k & 1 == 1 {
            r = mul(r, b);
        }
        b = mul(b, b);
        k >>= 1;
    }
    r
}

fn sub(a: u64, b: u64) -> u64 {
    if a >= b { a - b } else { a + P - b }
}

/// The base [`PolyHash`] uses. Fixed, so hashes agree across processes,
/// machines and files; and so not secret: this notices change, it doesn't
/// defend against someone crafting a collision.
pub const BASE: u64 = 0x0d1c_7a3b_92e4_f5a7;

/// A polynomial hash of a sequence of digits, modulo the prime 2^61 - 1,
/// kept with `BASE^len` so that two combine: `a.then(b)` is the hash of
/// `a`'s digits followed by `b`'s, however the sequence was split. That
/// makes it a summary: a tree's root hash depends only on its items, not on
/// its shape, so two trees with the same items agree whatever edits built
/// them.
///
/// It can also be undone: from the hashes of `prefix + rest` and of
/// `prefix`, [`without_prefix`](Self::without_prefix) gives the hash of
/// `rest`. So two prefix hashes give any range's hash.
///
/// Two different sequences of `n` digits collide with probability about
/// `n / 2^61`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PolyHash {
    value: u64,
    /// `BASE^len`, which also tells sequences of different lengths apart.
    shift: u64,
}

impl PolyHash {
    /// The hash of no digits.
    pub const EMPTY: PolyHash = PolyHash { value: 0, shift: 1 };

    /// The hash of these digits, each reduced modulo the prime.
    pub fn of(digits: impl IntoIterator<Item = u64>) -> Self {
        let mut out = Self::EMPTY;
        for d in digits {
            out.value = add(mul(out.value, BASE), d % P);
            out.shift = mul(out.shift, BASE);
        }
        out
    }

    /// These digits, then `next`'s.
    pub fn then(self, next: Self) -> Self {
        PolyHash {
            value: add(mul(self.value, next.shift), next.value),
            shift: mul(self.shift, next.shift),
        }
    }

    /// Given that `self` is the hash of `prefix` then `len` more digits,
    /// the hash of those `len` digits.
    pub fn without_prefix(self, prefix: Self, len: u64) -> Self {
        let shift = pow(BASE, len);
        PolyHash {
            value: sub(self.value, mul(prefix.value, shift)),
            shift,
        }
    }

    /// The hash as a number. Compare whole `PolyHash`es where you can: the
    /// number alone can't tell `[0, 5]` from `[5]`.
    pub fn value(self) -> u64 {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn digits() -> impl Strategy<Value = Vec<u64>> {
        prop::collection::vec(prop_oneof![0u64..300, any::<u64>()], 0..60)
    }

    proptest! {
        /// Splitting anywhere and combining gives the hash of the whole.
        #[test]
        fn combines_across_any_split(d in digits(), at in any::<prop::sample::Index>()) {
            let k = if d.is_empty() { 0 } else { at.index(d.len() + 1) };
            let (a, b) = d.split_at(k);
            prop_assert_eq!(PolyHash::of(a.iter().copied()).then(PolyHash::of(b.iter().copied())), PolyHash::of(d.iter().copied()));
        }

        /// The value is the plain polynomial hash in `BASE`.
        #[test]
        fn is_the_polynomial_hash(d in prop::collection::vec(0u64..P, 0..60)) {
            prop_assert_eq!(PolyHash::of(d.iter().copied()).value(), poly_hash(&d, BASE).unwrap());
        }

        /// Removing a prefix gives the hash of what's left.
        #[test]
        fn a_prefix_comes_off(d in digits(), at in any::<prop::sample::Index>()) {
            let k = if d.is_empty() { 0 } else { at.index(d.len() + 1) };
            let whole = PolyHash::of(d.iter().copied());
            let prefix = PolyHash::of(d[..k].iter().copied());
            let rest = PolyHash::of(d[k..].iter().copied());
            prop_assert_eq!(whole.without_prefix(prefix, (d.len() - k) as u64), rest);
        }
    }

    #[test]
    fn lengths_tell_leading_zeros_apart() {
        let (a, b) = (PolyHash::of([0, 5]), PolyHash::of([5]));
        assert_eq!(a.value(), b.value());
        assert_ne!(a, b);
    }
}
