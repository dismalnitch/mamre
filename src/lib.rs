pub mod hash;

#[macro_use(Clone, Copy)]
pub trait Item<T> {
    fn read(&self) -> T;
    fn insert(&self, item: &T) -> Result<usize, usize>;
}

pub trait Summary {}

pub trait Dimension<T> {}

pub struct Tree {}

impl Tree {
    pub fn push() {}
    pub fn seek() {}
}
