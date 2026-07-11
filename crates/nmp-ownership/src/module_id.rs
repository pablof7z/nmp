//! The module-id newtype used by [`crate::KindClaim::owner`].

use std::fmt;

/// A protocol module's stable identifier (e.g. `"nip17"`, `"nip29"`,
/// `"drafts"`). A thin newtype instead of a bare `&'static str` so a
/// module id is a distinct, greppable type rather than an easily-confused
/// string parameter (routing-and-ownership.md §4.1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ModuleId(pub &'static str);

impl ModuleId {
    /// `const fn` so a module's `claims()` table (plain static data, no
    /// macro required) can build `ModuleId`s at compile time.
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_id_display_and_const_new() {
        const NIP17: ModuleId = ModuleId::new("nip17");
        assert_eq!(NIP17.0, "nip17");
        assert_eq!(NIP17.to_string(), "nip17");
        assert_eq!(ModuleId::new("nip29"), ModuleId("nip29"));
    }
}
