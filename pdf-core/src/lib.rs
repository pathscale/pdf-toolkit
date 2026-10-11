#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod crypt;
pub mod document;
pub mod encryption;
pub mod error;
#[cfg(feature = "std")]
pub mod file;
pub mod filter;
pub mod lex;
pub mod limits;
pub mod object;
pub mod objstm;
pub mod pages;
pub mod repair;
pub mod source;
pub mod work;
pub mod xref;

pub use document::{Document, OpenMode, Resolver};
pub use error::{DocError, Warning};
pub use limits::{LimitKind, Limits, Refusal};
pub use object::{
    parse_indirect, parse_value, ArrayRef, DictRef, Name, Object, PdfStr, Value, ValueKind,
    ValueRef,
};
pub use source::PdfSource;
pub use work::{ObjectRef, ObjectWork, WorkBudget};
pub use xref::{Xref, XrefCounts, XrefEntry, XrefKind};
