#![allow(dead_code, deprecated)]

use std::marker::PhantomData;

#[deprecated(note = "fixture deprecation must be captured")]
#[must_use]
pub struct Outer<T>
where
    T: Clone,
{
    pub nested: deep_alias::Nested<T>,
    private: PrivateRoute,
    marker: PhantomData<T>,
}

struct PrivateRoute {
    hop: u16,
}

impl<T> Outer<T>
where
    T: Clone,
{
    pub const REVISION: u32 = 0;

    pub fn new(nested: deep_alias::Nested<T>) -> Self {
        Self {
            nested,
            private: PrivateRoute { hop: 0 },
            marker: PhantomData,
        }
    }

    #[deprecated(note = "fixture method deprecation must be captured")]
    #[must_use]
    pub fn map<U>(self, transform: impl FnOnce(T) -> U)
    where
        U: Clone,
    {
        let _ = transform(self.nested.value);
    }
}

pub type PublicAlias<T> = deep_alias::Nested<T>;

pub struct CycleA {
    pub next: Option<Box<CycleB>>,
}

pub struct CycleB {
    pub next: Option<Box<CycleA>>,
}

#[must_use]
pub fn generic_transform<T>(value: T) -> deep_alias::Nested<T>
where
    T: Clone + Send,
{
    deep_alias::Nested::new(value)
}

pub fn unrelated_dependency_api() -> bool {
    true
}
