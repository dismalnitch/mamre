//! Order keys: names for the walk's records that sort in document order, so
//! a record can be found by key in the tree, and the ordering rule can
//! compare where two origins are without looking up either.
//!
//! A key is a sequence of digits, compared like a word: digit by digit,
//! with a prefix before anything longer. A new record between two others
//! gets a key between theirs, and there's always one, because no key ends
//! in 0. Records never move, so a key never changes.
//!
//! The base document's items have keys `[1, k + 1]` for item `k`, so a run
//! of them needs no keys stored, and splitting one never mints any.
//!
//! The digit chosen leaves room for what usually comes next: just after the
//! lower key when appending (so typing forward stays at one length), half
//! way when squeezed in, and the middle of the range when starting a new
//! level. Typing backwards at one spot still lengthens keys, but by one
//! digit per ~64 inserts.

use std::fmt;
use std::sync::Arc;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Key(Arc<[u64]>);

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.iter()).finish()
    }
}

impl Key {
    /// The key of the base document's item `k`.
    pub fn base_item(k: usize) -> Key {
        Key(Arc::from([1, k as u64 + 1]))
    }

    /// A key strictly between `a` and `b`; `None` for `a` is before every
    /// key, and for `b` after every key. Needs `a < b`.
    pub fn between(a: Option<&Key>, b: Option<&Key>) -> Key {
        let a: &[u64] = a.map_or(&[], |k| &k.0);
        let b: Option<&[u64]> = b.map(|k| &*k.0);
        debug_assert!(b.is_none_or(|b| a < b), "between needs a < b");

        let mut out = Vec::new();
        // Whether `out` still equals `a` (or `b`) so far, so `a`'s (or
        // `b`'s) next digit bounds this one.
        let mut a_tight = true;
        let mut b_tight = b.is_some();
        for i in 0.. {
            // `None` once `a` runs out while `out` equals it: anything
            // longer is above it, so there's no lower bound left.
            let lo = if a_tight { a.get(i).copied() } else { None };
            // `b` can't run out while `out` equals it: `out` would be `b`,
            // or `b` a prefix of `a`.
            let hi = if b_tight { Some(b.unwrap()[i]) } else { None };

            let finish = match (lo, hi) {
                (None, None) => Some(1 << 63),
                (Some(l), None) => l.checked_add(1),
                (None, Some(h)) => (h >= 2).then_some(h / 2),
                (Some(l), Some(h)) => (h - l >= 2).then(|| l + (h - l) / 2),
            };
            if let Some(d) = finish {
                out.push(d);
                return Key(out.into());
            }
            // No room for a last digit here: take the lowest digit that
            // stays inside, and go a level deeper.
            let d = lo.unwrap_or(0);
            out.push(d);
            a_tight = lo == Some(d);
            b_tight = b_tight && hi == Some(d);
        }
        unreachable!("the loop returns")
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn key(digits: &[u64]) -> Key {
        Key(digits.into())
    }

    proptest! {
        /// Keys made by `between` from random gaps among existing keys stay
        /// strictly ordered.
        #[test]
        fn between_is_strictly_between(picks in prop::collection::vec(any::<prop::sample::Index>(), 1..300)) {
            let mut keys: Vec<Key> = vec![Key::base_item(0), Key::base_item(1)];
            for pick in picks {
                let gap = pick.index(keys.len() + 1);
                let a = gap.checked_sub(1).map(|i| &keys[i]);
                let b = keys.get(gap);
                let k = Key::between(a, b);
                if let Some(a) = a { prop_assert!(a < &k, "{a:?} < {k:?}"); }
                if let Some(b) = b { prop_assert!(&k < b, "{k:?} < {b:?}"); }
                keys.insert(gap, k);
            }
        }
    }

    #[test]
    fn typing_forward_stays_short() {
        let right = Key::base_item(6);
        let mut last = Key::base_item(5);
        for _ in 0..10_000 {
            last = Key::between(Some(&last), Some(&right));
        }
        assert_eq!(last.len(), 3);
    }

    #[test]
    fn typing_backwards_grows_slowly() {
        let left = Key::base_item(5);
        let mut first = Key::base_item(6);
        for _ in 0..640 {
            first = Key::between(Some(&left), Some(&first));
        }
        assert!(first.len() <= 14, "{} digits", first.len());
    }

    #[test]
    fn edges() {
        assert!(Key::between(None, Some(&key(&[1]))) < key(&[1]));
        assert!(Key::between(Some(&key(&[1])), Some(&key(&[1, 0, 1]))) > key(&[1]));
        assert!(Key::between(Some(&key(&[u64::MAX])), None) > key(&[u64::MAX]));
    }
}
