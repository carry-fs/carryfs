use std::{fmt, io};

/// Failure at a device, format, or volume boundary.
#[derive(Debug)]
pub enum Error {
    /// The underlying operation failed, possibly after writing durable bytes.
    Io {
        /// Device or host operation that failed.
        operation: &'static str,
        /// Original operating-system error.
        source: io::Error,
    },
    /// Sector sizes or image length are unsupported.
    InvalidGeometry,
    /// A request violates a checked precondition.
    InvalidInput(&'static str),
    /// Bytes on disk violate the supported format.
    Corrupt(&'static str),
    /// A structural invariant failed at a particular disk block.
    Corruption {
        /// Block containing the damaged record or reference.
        block: u64,
        /// Invariant that failed validation.
        reason: &'static str,
    },
    /// A checksummed record requires an unsupported format or feature.
    UnsupportedFormat,
    /// The device was opened without write access.
    ReadOnly,
    /// Another cooperating instance owns this image.
    Busy,
    /// There is not enough previously mirrored free space for this publication.
    NoSpace,
    /// A bounded operation exceeded its work or memory budget before output.
    LimitExceeded,
    /// Publication failed or corruption was detected; mutation requires reopen.
    Faulted,
    /// The volume has released its device.
    Ejected,
    /// A supplied name fails portability or normalization rules.
    InvalidName(&'static str),
    /// A supplied name or symlink target exceeds its length limit.
    TooLong(&'static str),
    /// The destination name is already in use and replacement is not allowed.
    NameCollision,
    /// No object exists at the requested name or ID.
    NotFound,
    /// The object exists but is not the expected type.
    WrongType,
    /// A directory removal or replacement target still has children.
    DirectoryNotEmpty,
    /// A directory cursor's namespace epoch no longer matches the volume.
    DirectoryChanged,
    /// A handle is stale, from another mount, or otherwise not valid here.
    InvalidHandle,
}

/// CarryFS result with device and corruption context.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn at_block(self, block: u64) -> Self {
        match self {
            Self::Corrupt(reason) => Self::Corruption { block, reason },
            error => error,
        }
    }
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{operation}: {source}"),
            Self::InvalidGeometry => f.write_str("unsupported device geometry"),
            Self::InvalidInput(reason) => write!(f, "invalid request: {reason}"),
            Self::Corrupt(reason) => write!(f, "corrupt CarryFS metadata: {reason}"),
            Self::Corruption { block, reason } => {
                write!(f, "corrupt CarryFS metadata at block {block}: {reason}")
            }
            Self::UnsupportedFormat => f.write_str("unsupported CarryFS format or feature"),
            Self::ReadOnly => f.write_str("device is read-only"),
            Self::Busy => f.write_str("image is already in use"),
            Self::NoSpace => f.write_str("insufficient free space"),
            Self::LimitExceeded => f.write_str("operation exceeds its work budget"),
            Self::Faulted => f.write_str("volume is faulted; close and reopen it"),
            Self::Ejected => f.write_str("volume has been ejected"),
            Self::InvalidName(reason) => write!(f, "invalid name: {reason}"),
            Self::TooLong(reason) => f.write_str(reason),
            Self::NameCollision => f.write_str("name is already in use"),
            Self::NotFound => f.write_str("no such object"),
            Self::WrongType => f.write_str("object is not the expected type"),
            Self::DirectoryNotEmpty => f.write_str("directory is not empty"),
            Self::DirectoryChanged => f.write_str("directory changed since the cursor was issued"),
            Self::InvalidHandle => f.write_str("handle is not valid for this volume"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
