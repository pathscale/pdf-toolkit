#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod data {
    pub mod encodings;
    pub mod standard14;
}

pub mod annots;
pub mod cmap;
pub mod content;
pub mod encoding;
pub mod extract;
pub mod font;
pub mod images;
pub mod interp;
pub mod layout;
pub mod links;
pub mod semantic;
pub mod standard14;

pub use extract::{extract_text, TextExtraction};
pub use font::{Font, FontFlags, FontKind, Glyph, GlyphText};
pub use interp::{
    display_transform, page_text, user_space_rect, user_space_transform, FontInfo, ImageInfo,
    PageText, PositionedGlyph,
};
pub use layout::{layout, Block, BlockKind, Line, PageLayout};
