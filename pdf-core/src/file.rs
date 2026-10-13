#![cfg(feature = "std")]

use crate::source::{PdfSource, SourceError};
use alloc::sync::Arc;
use core::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Errors returned while opening a PDF source from a path.
#[derive(Debug)]
pub enum FileOpenError {
    Io(std::io::Error),
    TooLarge { limit: u64, observed: u64 },
    Source(SourceError),
}

impl fmt::Display for FileOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not read PDF file: {error}"),
            Self::TooLarge { limit, observed } => write!(
                formatter,
                "PDF file exceeded the {limit} byte limit after {observed} bytes were observed"
            ),
            Self::Source(error) => write!(formatter, "invalid PDF source: {error}"),
        }
    }
}

impl std::error::Error for FileOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::TooLarge { .. } => None,
        }
    }
}

/// Reads a file into a PDF source without reading beyond the caller's limit.
pub fn open(path: impl AsRef<Path>, max_bytes: u64) -> Result<PdfSource, FileOpenError> {
    open_with_mode(path, max_bytes, false)
}

/// Reads a bounded source, recognizing damaged headers for repair opens.
pub fn open_repair(path: impl AsRef<Path>, max_bytes: u64) -> Result<PdfSource, FileOpenError> {
    open_with_mode(path, max_bytes, true)
}

fn open_with_mode(
    path: impl AsRef<Path>,
    max_bytes: u64,
    repair: bool,
) -> Result<PdfSource, FileOpenError> {
    let file = File::open(path).map_err(FileOpenError::Io)?;
    let file_size = file.metadata().map_err(FileOpenError::Io)?.len();
    if file_size > max_bytes {
        return Err(FileOpenError::TooLarge {
            limit: max_bytes,
            observed: file_size,
        });
    }

    // Read straight into the shared allocation: a Vec then Arc::from would hold the
    // file twice at peak. repeat_n is TrustedLen, so collect allocates exactly once.
    let length = usize::try_from(file_size).map_err(|_| FileOpenError::TooLarge {
        limit: max_bytes,
        observed: file_size,
    })?;
    let mut bytes: Arc<[u8]> = core::iter::repeat_n(0u8, length).collect();
    let mut file = file;
    match Arc::get_mut(&mut bytes) {
        Some(buffer) => file.read_exact(buffer).map_err(FileOpenError::Io)?,
        None => unreachable!("a freshly collected Arc is unique"),
    }

    if repair {
        PdfSource::new_repair(bytes).map_err(FileOpenError::Source)
    } else {
        PdfSource::new(bytes).map_err(FileOpenError::Source)
    }
}
