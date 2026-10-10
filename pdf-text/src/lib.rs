#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod data {
    pub mod encodings;
    pub mod standard14;
}

pub mod font;
pub mod encoding;
pub mod cmap;
pub mod standard14;
pub mod content;
pub mod interp;
pub mod layout;
pub mod extract;
pub mod semantic;
pub mod links;
pub mod images;

pub use font::{Font, FontFlags, FontKind, Glyph, GlyphText};
pub use interp::{page_text, FontInfo, ImageInfo, PageText, PositionedGlyph};
pub use layout::{layout, Block, BlockKind, Line, PageLayout};
pub use extract::{extract_text, TextExtraction};

