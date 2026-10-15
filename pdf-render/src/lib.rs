#![no_std]

extern crate alloc;

mod calculator;

pub mod display;
pub mod glyphs;
pub mod paint;
pub mod sfnt;
pub mod type1;

pub use display::{
    display_list, Dash, DisplayList, FallbackFonts, FillRule, GlyphPath, ImageSource, Item, Matrix,
    Paint, Path, PathEl, RenderOptions, Rgba, Stroke,
};
