pub mod hash;
pub mod tree;

pub trait Item: Clone {
    type Summary: Summary;
    fn summary(&self) -> Self::Summary;
}

pub trait Summary {
    fn empty() -> Self;
    fn concat(&mut self, other: &Self);
}

pub trait Dimension<S: Summary>: Ord + Clone {
    fn zero() -> Self;
    fn add_summary(&mut self, summary: &S);
}
