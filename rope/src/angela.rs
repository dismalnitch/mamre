//! Angela: a content hash of the text, kept in the tree like any summary,
//! so a rope can say whether any range has changed in O(log n), and two
//! ropes on two machines can compare without sending their text.
//!
//! The hash depends only on the bytes, never on how they're chunked or how
//! the tree is shaped, so two replicas that reached the same text through
//! different edits agree. Within one program, [`Rope::ptr_eq`] is cheaper
//! still, for "is this the very rope I laid out last time?".
//!
//! Angela notices change; she isn't a defence against someone crafting a
//! collision. Two different texts of `n` bytes collide with probability
//! about `n / 2^61`.

use std::ops::Range;

use mamre::hash::PolyHash;

use crate::summary::TextSummary;
use crate::{Measure, Rope};

/// A content hash of the text: its bytes as the digits of a polynomial
/// hash. Put it in a rope's measure, as in `Rope<(Sections, Angela)>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Angela(PolyHash);

impl Angela {
    /// The hash of `text`, as a rope would have it: the way to hash text
    /// that isn't in a rope, to compare with one.
    pub fn of(text: &str) -> Self {
        // Each byte as a digit from 1, so a run of zero bytes still counts.
        Angela(PolyHash::of(text.bytes().map(|b| b as u64 + 1)))
    }

    /// The hash as a number, for storing or sending. Compare whole
    /// `Angela`s where you can: they also tell lengths apart.
    pub fn value(self) -> u64 {
        self.0.value()
    }
}

impl Measure for Angela {
    fn empty() -> Self {
        Angela(PolyHash::EMPTY)
    }

    fn measure(text: &str) -> Self {
        Angela::of(text)
    }

    fn concat(&mut self, next: &Self) {
        self.0 = self.0.then(next.0);
    }
}

fn angela<M: Measure>(summary: &TextSummary<M>) -> Angela {
    *summary.units.find::<Angela>().unwrap_or_else(|| {
        panic!("this rope's measure doesn't keep Angela; add her, as in Rope<(Sections, Angela)>")
    })
}

impl<M: Measure, const N: usize> Rope<M, N> {
    /// The hash of the whole text. O(1): it's at the root.
    ///
    /// Panics, like `hash_range`, if the rope's measure doesn't keep
    /// [`Angela`].
    pub fn hash(&self) -> Angela {
        angela(self.summary())
    }

    /// The hash of the text in `range`: two prefix hashes, and the first
    /// taken off the second. O(log n), so "has section 3 changed?" is two
    /// seeks.
    pub fn hash_range(&self, range: Range<usize>) -> Angela {
        self.check_range(&range);
        let end = angela(&self.summary_before(range.end));
        let start = angela(&self.summary_before(range.start));
        Angela(end.0.without_prefix(start.0, range.len() as u64))
    }

    /// Whether `other` is the very same rope, not just an equal one: true
    /// for a clone until either changes. O(1).
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.tree.ptr_eq(&other.tree)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::Sections;

    fn text() -> impl Strategy<Value = String> {
        let pool = vec!['a', 'b', '\0', ' ', '\n', '#', 'é', '日', '🦀'];
        prop::collection::vec(prop::sample::select(pool), 0..300)
            .prop_map(|cs| cs.into_iter().collect())
    }

    proptest! {
        /// Measuring the pieces of any split and combining gives the hash of
        /// the whole.
        #[test]
        fn hashes_combine_across_any_split(
            text in text(),
            cuts in prop::collection::vec(any::<u16>(), 0..6),
        ) {
            let mut sum = Angela::empty();
            for range in crate::measure::tests::pieces(&text, &cuts) {
                sum.concat(&Angela::of(&text[range]));
            }
            prop_assert_eq!(sum, Angela::of(&text));
        }

        /// The same text reached two different ways, so chunked and shaped
        /// differently, hashes the same; and any range hashes as its text.
        #[test]
        fn depends_on_the_text_only(text in text(), split in any::<u16>(), a in any::<u16>(), b in any::<u16>()) {
            let cut = |f: u16| {
                let mut i = text.len() * f as usize / u16::MAX as usize;
                while !text.is_char_boundary(i) {
                    i -= 1;
                }
                i
            };
            let whole: Rope<(Sections, Angela), 4> = Rope::from(text.as_str());
            let mut built: Rope<(Sections, Angela), 4> = Rope::from(&text[cut(split)..]);
            built.insert(0, &text[..cut(split)]);
            prop_assert_eq!(whole.hash(), Angela::of(&text));
            prop_assert_eq!(built.hash(), whole.hash());

            let (a, b) = (cut(a).min(cut(b)), cut(a).max(cut(b)));
            prop_assert_eq!(whole.hash_range(a..b), Angela::of(&text[a..b]));
            prop_assert_eq!(built.hash_range(a..b), Angela::of(&text[a..b]));
        }
    }

    #[test]
    fn which_section_changed() {
        let before = "# One\nfirst\n# Two\nsecond\n# Three\nthird\n";
        let old: Rope<(Sections, Angela), 8> = Rope::from(before);
        let mut new = old.clone();
        assert!(new.ptr_eq(&old));
        let at = before.find("second").unwrap();
        new.replace(at..at + "second".len(), "SECOND");
        assert!(!new.ptr_eq(&old));

        // Compare each section's hash: only Two differs.
        let unit = crate::Unit::Section;
        let changed: Vec<usize> = (0..old.count(unit))
            .filter(|&n| {
                let (a, b) = (
                    old.unit_range(n, unit).unwrap(),
                    new.unit_range(n, unit).unwrap(),
                );
                old.hash_range(a) != new.hash_range(b)
            })
            .collect();
        assert_eq!(changed, [1]);
    }

    #[test]
    fn a_run_of_zero_bytes_still_counts() {
        assert_ne!(Angela::of("\0"), Angela::of(""));
        assert_ne!(Angela::of("\0a").value(), Angela::of("a").value());
    }
}
