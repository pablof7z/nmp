#![allow(dead_code)]

pub struct Nested<T>
where
    T: Clone,
{
    pub value: T,
    pub revision: u32,
    pub route: RouteAlias,
    pub mixed: Mixed,
}

pub struct Mixed(pub u8, u8);

pub type RouteAlias = RouteState;

pub enum RouteState {
    Ready,
    Failed(u16),
}

pub enum AlternateState {
    Alternate,
}

impl<T> Nested<T>
where
    T: Clone,
{
    pub const REVISION: u32 = 0;

    pub fn new(value: T) -> Self {
        Self {
            value,
            revision: 0,
            route: RouteState::Ready,
            mixed: Mixed(1, 2),
        }
    }

    #[deprecated(note = "fixture method deprecation must be captured")]
    #[must_use]
    pub fn map<U>(self, transform: impl FnOnce(T) -> U) -> Nested<U>
    where
        U: Clone,
    {
        Nested::new(transform(self.value))
    }

    pub fn helper_only(&self) {}
}
