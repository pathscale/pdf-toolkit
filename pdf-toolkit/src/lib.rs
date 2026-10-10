//! One crate for the whole toolkit. Each module is one of the layered crates,
//! re-exported behind a feature so a reader-only build stays small and no_std.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let bytes = std::fs::read("book.pdf")?;
//! let source = pdf_toolkit::core::PdfSource::new(bytes)?;
//! let text = pdf_toolkit::text::extract::extract_text(source, pdf_toolkit::core::Limits::default())?;
//! println!("{}", text.text);
//! # Ok(())
//! # }
//! ```
#![no_std]

/// Objects, xref, filters, page tree and named limits (`pdf-toolkit-core`).
pub use pdf_core as core;

/// Fonts, text extraction, reading order, semantic documents, images (`pdf-toolkit-text`).
#[cfg(feature = "text")]
pub use pdf_text as text;

/// Page display lists and glyph outlines (`pdf-toolkit-render`).
#[cfg(feature = "render")]
pub use pdf_render as render;

/// Rasterizing pages to RGBA and PNG (`pdf-toolkit-raster`).
#[cfg(feature = "raster")]
pub use pdf_raster as raster;

/// The deterministic document writer (`pdf-toolkit-write`).
#[cfg(feature = "write")]
pub use pdf_write as write;

/// Incremental updates that keep the original bytes (`pdf-toolkit-edit`).
#[cfg(feature = "edit")]
pub use pdf_edit as edit;
