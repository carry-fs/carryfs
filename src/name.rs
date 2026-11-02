use std::cell::RefCell;

use carryfs_ondisk::name::{self as profile, Scratch};

use crate::Result;

thread_local! {
    // About 33 KiB, so one per thread instead of one per name.
    static SCRATCH: RefCell<Box<Scratch>> = RefCell::new(Box::new(Scratch::new()));
}

fn with_scratch<T>(f: impl FnOnce(&mut Scratch) -> T) -> T {
    SCRATCH.with(|scratch| f(&mut scratch.borrow_mut()))
}

/// A validated, normalized directory-entry name.
///
/// Holds the NFC stored spelling and the folded comparison key used as the
/// catalog directory-entry key suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub(crate) stored: Vec<u8>,
    pub(crate) folded: Vec<u8>,
}

impl Name {
    /// Parse and validate a name from an adapter's decoded input string.
    ///
    /// Performs NFC normalization, full case folding, and the pinned
    /// filename-profile checks in FORMAT.md. Native adapters decode their own
    /// input string encoding and then call this same parser.
    pub fn parse(input: &str) -> Result<Self> {
        with_scratch(|scratch| {
            let name = profile::parse(input, scratch)?;
            Ok(Self {
                stored: name.stored.to_vec(),
                folded: name.folded.to_vec(),
            })
        })
    }

    /// The stored NFC spelling, as bytes.
    pub fn stored(&self) -> &[u8] {
        &self.stored
    }

    /// Whether Windows reserves this name, including names with extensions.
    pub fn is_windows_device_name(&self) -> bool {
        profile::is_reserved_device_name(&self.folded)
    }
}

/// Validates a stored directory name and its pinned comparison key.
pub(crate) fn validate_disk_name(stored: &[u8], folded: &[u8]) -> Result<()> {
    with_scratch(|scratch| Ok(profile::validate_disk_name(stored, folded, scratch)?))
}

#[cfg(test)]
mod tests {
    use super::Name;

    // carryfs-ondisk tests the profile itself; these check the wrapper.
    #[test]
    fn parse_normalizes_and_folds() {
        let name = Name::parse("Straße").unwrap();
        assert_eq!(name.stored, "Straße".as_bytes());
        assert_eq!(name.folded, "strasse".as_bytes());
        assert!(Name::parse("CON.txt").unwrap().is_windows_device_name());
        assert!(Name::parse("a/b").is_err());
    }
}
