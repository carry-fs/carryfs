//! Failures of the pure encoding checks.

/// A format or input violation found without touching a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Sector sizes or image length are unsupported.
    InvalidGeometry,
    /// A request violates a checked precondition.
    InvalidInput(&'static str),
    /// Bytes on disk violate the supported format.
    Corrupt(&'static str),
    /// A checksummed record requires an unsupported format or feature.
    UnsupportedFormat,
    /// A supplied name fails portability or normalization rules.
    InvalidName(&'static str),
    /// A supplied name exceeds its length limit.
    TooLong(&'static str),
}

/// Result of an encoding check.
pub type Result<T> = core::result::Result<T, Error>;
