//! Error type shared by the image parsers.

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The underlying stream failed or returned fewer bytes than requested.
    Io,
    /// The image does not carry the volume structure this reader looks for.
    NoVolume,
    /// The image does not have the structure the parser expected.
    Corrupt(&'static str),
    /// A structure that is valid but that this reader deliberately does not handle.
    Unsupported(&'static str),
    /// No thumbnail (or the requested directory/file) exists in the image.
    NotFound,
    /// The data exceeds one of the safety limits.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io => f.write_str("read error"),
            Error::NoVolume => f.write_str("no UDF or ISO 9660 volume found"),
            Error::Corrupt(what) => write!(f, "corrupt image: {what}"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::NotFound => f.write_str("no thumbnail found in image"),
            Error::TooLarge => f.write_str("data exceeds safety limit"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
