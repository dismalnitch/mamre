//! Edits, built only from the public primitives: a cursor's `slice`,
//! `seek_forward` and `suffix`, and the tree's `push` and `append`. Nothing
//! here reaches into nodes, which is the point: if an edit can't be written
//! this way, a primitive is missing.
//!
//! Positions are seek targets with a bias, as for a cursor, so an edit can
//! land by item count, by length, or by any other dimension. Item `n` is
//! `&Count(n)` with `Bias::Right`. When a target type works with more than
//! one dimension, name the one you mean: `tree.insert::<Count>(...)`.
//!
//! Each edit cuts and rejoins in O(log n) cuts and joins, and shares every
//! untouched subtree with the tree it came from.

use crate::{Dimension, Item, SeekTarget, SumTree, cursor::Bias};

impl<T: Item, const CAP: usize> SumTree<T, CAP> {
    /// Replaces everything from where a seek for `from` lands up to where a
    /// seek for `to` lands with `items`. The same `bias` places both ends.
    ///
    /// A `to` behind `from` removes nothing, so `items` go in at `from`.
    /// [`insert`](Self::insert) and [`remove`](Self::remove) are this with
    /// one side empty.
    pub fn replace<D: Dimension<T::Summary>>(
        &mut self,
        from: &impl SeekTarget<T::Summary, D>,
        to: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
        items: impl IntoIterator<Item = T>,
    ) {
        let (mut out, rest) = {
            let mut cursor = self.cursor::<D>();
            let before = cursor.slice(from, bias);
            // Step over what's being replaced; never moves back.
            cursor.seek_forward(to, bias);
            (before, cursor.suffix())
        };
        out.extend(items);
        out.append(rest);
        *self = out;
    }

    /// Puts `items` in where a seek for `at` lands, before the item there.
    pub fn insert<D: Dimension<T::Summary>>(
        &mut self,
        at: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
        items: impl IntoIterator<Item = T>,
    ) {
        self.replace(at, at, bias, items);
    }

    /// Removes everything from where a seek for `from` lands up to where a
    /// seek for `to` lands. A `to` behind `from` removes nothing.
    pub fn remove<D: Dimension<T::Summary>>(
        &mut self,
        from: &impl SeekTarget<T::Summary, D>,
        to: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
    ) {
        self.replace(from, to, bias, []);
    }

    /// Cuts the tree where a seek for `at` lands: keeps what's before, and
    /// returns the rest.
    pub fn split_off<D: Dimension<T::Summary>>(
        &mut self,
        at: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
    ) -> SumTree<T, CAP> {
        let (before, rest) = {
            let mut cursor = self.cursor::<D>();
            (cursor.slice(at, bias), cursor.suffix())
        };
        *self = before;
        rest
    }

    /// Keeps only what's before where a seek for `at` lands.
    pub fn truncate<D: Dimension<T::Summary>>(
        &mut self,
        at: &impl SeekTarget<T::Summary, D>,
        bias: Bias,
    ) {
        *self = self.cursor::<D>().slice(at, bias);
    }

    /// Adds all of `other` before this tree's items: `append`, the other way.
    pub fn prepend(&mut self, other: SumTree<T, CAP>) {
        let back = std::mem::replace(self, other);
        self.append(back);
    }
}

/// Adds the items onto the end: built into a tree of their own at once
/// (see [`SumTree::from_items`]), then appended.
impl<T: Item, const CAP: usize> Extend<T> for SumTree<T, CAP> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        self.append(SumTree::from_items(items));
    }
}

/// So `iter.collect::<SumTree<_>>()` works: built at once, see
/// [`SumTree::from_items`].
impl<T: Item, const CAP: usize> FromIterator<T> for SumTree<T, CAP> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        SumTree::from_items(items)
    }
}

#[cfg(test)]
mod tests {
    //! Every edit against the same edit on a `Vec`, on tiny nodes so trees
    //! are several levels tall, checking shape and summary too, and that a
    //! copy taken before the edit is untouched.

    use proptest::prelude::*;

    use super::*;
    use crate::Summary;

    #[derive(Clone, Debug, PartialEq)]
    struct Totals {
        count: usize,
        sum: u64,
    }

    impl Summary for Totals {
        fn empty() -> Self {
            Totals { count: 0, sum: 0 }
        }
        fn concat(&mut self, other: &Self) {
            self.count += other.count;
            self.sum += other.sum;
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct N(u64);

    impl Item for N {
        type Summary = Totals;
        fn summary(&self) -> Totals {
            Totals {
                count: 1,
                sum: self.0,
            }
        }
    }

    /// Item count: an ordered dimension, so it's its own seek target.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Count(usize);

    impl Dimension<Totals> for Count {
        fn zero() -> Self {
            Count(0)
        }
        fn add_summary(&mut self, s: &Totals) {
            self.0 += s.count;
        }
    }

    type Tree = SumTree<N, 4>;

    fn values(tree: &Tree) -> Vec<u64> {
        tree.iter().map(|n| n.0).collect()
    }

    /// The tree holds `want`, keeps its shape rules, and sums right.
    fn holds(tree: &Tree, want: &[u64]) -> Result<(), TestCaseError> {
        tree.check().map_err(TestCaseError::fail)?;
        prop_assert_eq!(values(tree), want);
        prop_assert_eq!(
            tree.summary(),
            &Totals {
                count: want.len(),
                sum: want.iter().sum()
            }
        );
        Ok(())
    }

    fn build(values: &[u64]) -> Tree {
        values.iter().map(|&v| N(v)).collect()
    }

    /// A start of 0..300 items, and two positions that may be out of order
    /// or past the end.
    fn setup() -> impl Strategy<Value = (Vec<u64>, usize, usize)> {
        prop::collection::vec(0u64..1000, 0..300).prop_flat_map(|v| {
            let n = v.len();
            (Just(v), 0..=n + 2, 0..=n + 2)
        })
    }

    proptest! {
        #[test]
        fn replace_matches_a_splice(
            (start, a, b) in setup(),
            new in prop::collection::vec(0u64..1000, 0..300),
        ) {
            let mut tree = build(&start);
            let copy = tree.clone();
            tree.replace::<Count>(&Count(a), &Count(b), Bias::Right, new.iter().map(|&v| N(v)));

            let mut want = start.clone();
            let from = a.min(want.len());
            let to = b.clamp(from, want.len());
            want.splice(from..to, new);
            holds(&tree, &want)?;
            holds(&copy, &start)?;
        }

        #[test]
        fn insert_matches_a_vec(
            (start, at, _) in setup(),
            new in prop::collection::vec(0u64..1000, 0..300),
        ) {
            let mut tree = build(&start);
            tree.insert::<Count>(&Count(at), Bias::Right, new.iter().map(|&v| N(v)));

            let mut want = start.clone();
            let at = at.min(want.len());
            want.splice(at..at, new);
            holds(&tree, &want)?;
        }

        /// Splicing in place, again and again, grows and splits nodes as a
        /// Vec would, and leaves a shared copy alone.
        #[test]
        fn splice_at_matches_a_vec(
            start in prop::collection::vec(0u64..1000, 1..60),
            edits in prop::collection::vec((any::<prop::sample::Index>(), 0u64..1000, 0usize..=4, any::<bool>()), 1..40),
        ) {
            let mut tree = build(&start);
            let copy = tree.clone();
            let mut want = start.clone();
            for (k, (at, value, more, before)) in edits.into_iter().enumerate() {
                let at = at.index(want.len());
                let new: Vec<u64> = (0..more as u64).map(|j| 5000 + 10 * k as u64 + j).collect();
                let out = tree.splice_at::<Count, _, _>(&Count(at), Bias::Right, |item| {
                    let old = item.0;
                    if before {
                        // In front: the first new one swaps in, the old
                        // item goes after the rest.
                        let mut put: Vec<N> = new.iter().map(|&v| N(v)).collect();
                        put.push(N(old));
                        let first = put.remove(0);
                        *item = first;
                        (old, put)
                    } else {
                        item.0 = value;
                        (old, new.iter().map(|&v| N(v)).collect())
                    }
                });
                prop_assert_eq!(out, Some(want[at]));
                if before {
                    let mut put = new.clone();
                    put.push(want[at]);
                    let first = put.remove(0);
                    want[at] = first;
                    want.splice(at + 1..at + 1, put);
                } else {
                    want[at] = value;
                    want.splice(at + 1..at + 1, new);
                }
                holds(&tree, &want)?;
            }
            holds(&copy, &start)?;
            prop_assert_eq!(
                tree.splice_at::<Count, _, _>(&Count(want.len()), Bias::Right, |_| ((), [])),
                None
            );
        }

        /// Removing in place, mixed with splicing, shrinks, borrows, merges
        /// and collapses as a Vec would, and leaves a shared copy alone.
        #[test]
        fn remove_at_matches_a_vec(
            start in prop::collection::vec(0u64..1000, 0..80),
            edits in prop::collection::vec((any::<prop::sample::Index>(), 0usize..=4), 1..80),
        ) {
            let mut tree = build(&start);
            let copy = tree.clone();
            let mut want = start.clone();
            for (k, (at, more)) in edits.into_iter().enumerate() {
                if want.is_empty() || more == 4 {
                    // Grow it again now and then, so there's more to remove.
                    let at = if want.is_empty() { 0 } else { at.index(want.len()) };
                    let new: Vec<u64> = (0..3).map(|j| 5000 + 10 * k as u64 + j).collect();
                    if want.is_empty() {
                        tree = build(&new);
                        want = new;
                    } else {
                        tree.splice_at::<Count, _, _>(&Count(at), Bias::Right, |_| {
                            ((), new.iter().map(|&v| N(v)).collect::<Vec<_>>())
                        });
                        want.splice(at + 1..at + 1, new);
                    }
                } else {
                    let at = at.index(want.len());
                    let gone = tree.remove_at::<Count>(&Count(at), Bias::Right);
                    prop_assert_eq!(gone.map(|n| n.0), Some(want.remove(at)));
                }
                holds(&tree, &want)?;
            }
            holds(&copy, &start)?;
            prop_assert!(tree.remove_at::<Count>(&Count(want.len()), Bias::Right).is_none());
        }

        #[test]
        fn remove_matches_a_vec((start, a, b) in setup()) {
            let mut tree = build(&start);
            tree.remove::<Count>(&Count(a), &Count(b), Bias::Right);

            let mut want = start.clone();
            let from = a.min(want.len());
            let to = b.clamp(from, want.len());
            want.drain(from..to);
            holds(&tree, &want)?;
        }

        #[test]
        fn split_off_truncate_and_prepend((start, at, _) in setup()) {
            let at_clamped = at.min(start.len());
            let (front, back) = start.split_at(at_clamped);

            let mut tree = build(&start);
            let rest = tree.split_off::<Count>(&Count(at), Bias::Right);
            holds(&tree, front)?;
            holds(&rest, back)?;

            // Putting the front back on the rest gives the start again.
            let mut whole = rest;
            whole.prepend(tree);
            holds(&whole, &start)?;

            let mut cut = build(&start);
            cut.truncate::<Count>(&Count(at), Bias::Right);
            holds(&cut, front)?;
        }

        /// `Left` lands on the item ending at a boundary, so `Count(n)` with
        /// `Left` is one item earlier than with `Right`: item `n - 1`. Except
        /// at 0, where there's no earlier item, and past the end, where
        /// there's no item to land on, so nothing is cut off.
        #[test]
        fn left_bias_cuts_one_earlier((start, at, _) in setup()) {
            let mut tree = build(&start);
            let rest = tree.split_off::<Count>(&Count(at), Bias::Left);
            let cut = if at > start.len() { start.len() } else { at.saturating_sub(1) };
            let (front, back) = start.split_at(cut);
            holds(&tree, front)?;
            holds(&rest, back)?;
        }
    }

    #[test]
    fn collect_and_extend() {
        let mut tree: Tree = (0..10).map(N).collect();
        tree.extend((10..20).map(N));
        assert_eq!(values(&tree), (0..20).collect::<Vec<_>>());
        tree.check().unwrap();
    }
}
