use crate::{Dimension, Item, Summary};
use std::sync::Arc;
use std::vec::Vec;

pub struct Tree<T: Item>(Arc<Node<T>>);

enum Node<T: Item> {
    Internal {
        summary: T::Summary,
        children: Vec<Tree<T>>,
        child_summaries: Vec<T::Summary>,
    },
    Leaf {
        summary: T::Summary,
        items: Vec<T>,
        item_summaries: Vec<T::Summary>,
    },
}
