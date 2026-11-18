use std::sync::Arc;

use crate::{ArrayVec, Item, Node, SumTree, Summary, TreeError, TreeResult, units::TREE_BASE};

impl<T: Item> SumTree<T> {
    pub fn push(&mut self, item: T) {
        // NOTE: root split
        // if let Some(right) = self.0.push(item) {
        // new root: Internal { children: [old root, right], ... }
        // let l_child =
        // Internal {
        //     // TODO: write root split
        //     children: [self.0, right],
        //     child_summaries: [root.su]
        // },
        // Leaf {
        //     summary.concat(&it.summary());
        //     child_summaries.push(it.summary());
        //     children.push(Node::Leaf(it));
        // }
        // }
    }
}

/// Moves the back half of `v` into a new vec and returns it; `v` keeps the
/// front half. The returned half gets the extra element when the length is odd.
fn split<T, const CAP: usize>(v: &mut ArrayVec<T, CAP>) -> TreeResult<ArrayVec<T, CAP>> {
    if v.len() <= 1 {
        return Err(TreeError::NoEffectError);
    }
    let mid = v.len() / 2;
    Ok(v.drain(mid..).collect())
}

impl<T: Item> Node<T> {
    pub fn push(&mut self, in_item: T) -> Option<SumTree<T>> {
        match self {
            Node::Internal {
                summary,
                children,
                child_summaries,
            } => {
                if children.is_full() {
                    todo!("AHHHHHH");
                } else {
                    Ok(children.last()?.push(in_item))
                }
            }
            Node::Leaf {
                summary,
                items,
                item_summaries,
            } => {
                if items.is_full() {
                    // Keep the left half in place; move the right half to a new sibling.
                    let mut right_items = split(items).expect("full leaf has >1 item");
                    let mut right_sums = split(item_summaries).expect("full leaf has >1 item");
                    right_sums.push(in_item.summary());
                    right_items.push(in_item);

                    *summary = T::Summary::empty();
                    for s in item_summaries.iter() {
                        summary.concat(s);
                    }
                    let mut right_summary = T::Summary::empty();
                    for s in right_sums.iter() {
                        right_summary.concat(s);
                    }

                    Some(SumTree(Arc::new(Node::Leaf {
                        summary: right_summary,
                        items: right_items,
                        item_summaries: right_sums,
                    })))
                } else {
                    let s = in_item.summary();
                    summary.concat(&s);
                    item_summaries.push(s);
                    items.push(in_item);
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn split_preserves_order_and_balance(v in prop::collection::vec(any::<u8>(), 2..=12)) {
            let mut l: ArrayVec<u8, 12> = v.iter().copied().collect();
            let r = split(&mut l).unwrap();
            prop_assert_eq!([&l[..], &r[..]].concat(), v);
            prop_assert!(l.len().abs_diff(r.len()) <= 1);
        }
    }

    #[test]
    fn split_too_small_is_no_effect() {
        let mut empty: ArrayVec<u8, 12> = ArrayVec::new();
        assert_eq!(split(&mut empty), Err(TreeError::NoEffectError));
        let mut one: ArrayVec<u8, 12> = [7].into_iter().collect();
        assert_eq!(split(&mut one), Err(TreeError::NoEffectError));
        assert_eq!(one.as_slice(), &[7]); // untouched on error
    }
}
