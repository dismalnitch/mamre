type TreeError;

#[macro_use(Clone, Copy)]
pub trait Item<T> {
    fn read(&self) -> T;
    fn insert(&self, item: &T) -> Result<usize, TreeError>;
}

pub trait Summary {}

pub trait Dimension<T> {}

pub struct Tree {}

impl Tree {
    pub fn push() {}
    pub fn seek() {}
}
