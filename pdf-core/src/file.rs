#![cfg(feature = "std")]

use crate::source::{PdfSource, SourceError};
use alloc::sync::Arc;
use alloc::vec::Vec;
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
    let file = File::open(path).map_err(FileOpenError::Io)?;
    let file_size = file.metadata().map_err(FileOpenError::Io)?.len();
    if file_size > max_bytes {
        return Err(FileOpenError::TooLarge {
            limit: max_bytes,
            observed: file_size,
        });
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(FileOpenError::Io)?;
    let observed = bytes.len() as u64;
    if observed > max_bytes {
        return Err(FileOpenError::TooLarge {
            limit: max_bytes,
            observed,
        });
    }

    PdfSource::new(Arc::<[u8]>::from(bytes)).map_err(FileOpenError::Source)
}

#[cfg(test)]
mod tests {
    use super::{open, FileOpenError};
    use std::fs::{OpenOptions, remove_file};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const HOUSE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n\
<< /Type /Catalog /Pages 2 0 R >>\n\
endobj\n\
2 0 obj\n\
<< /Type /Pages /Kids [] /Count 0 >>\n\
endobj\n\
xref\n\
0 3\n\
0000000000 65535 f \n\
0000000009 00000 n \n\
0000000058 00000 n \n\
trailer\n\
<< /Size 3 /Root 1 0 R >>\n\
startxref\n\
110\n\
%%EOF\n";

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    struct TempFile {
        path: PathBuf,
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = remove_file(&self.path);
        }
    }

    fn write_house_fixture() -> io::Result<TempFile> {
        loop {
            let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(std::format!(
                "pdf-core-house-fixture-{}-{id}.pdf",
                std::process::id()
            ));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let temporary_file = TempFile { path };
            file.write_all(HOUSE_PDF)?;
            return Ok(temporary_file);
        }
    }

    #[test]
    fn file_open_reads_house_fixture() -> Result<(), std::boxed::Box<dyn std::error::Error>> {
        let file = write_house_fixture()?;
        let source = open(&file.path, HOUSE_PDF.len() as u64)?;

        assert_eq!(source.bytes(), HOUSE_PDF);
        assert_eq!(source.header().version().major(), 1);
        assert_eq!(source.header().version().minor(), 4);
        Ok(())
    }

    #[test]
    fn file_open_refuses_oversize() -> Result<(), std::boxed::Box<dyn std::error::Error>> {
        let file = write_house_fixture()?;
        let limit = HOUSE_PDF.len() as u64 - 1;
        let error = match open(&file.path, limit) {
            Ok(_) => return Err("oversized file was accepted".into()),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            FileOpenError::TooLarge { limit: found_limit, observed }
                if found_limit == limit && observed > limit
        ));
        Ok(())
    }
}
