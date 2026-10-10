#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod lex;
pub mod object;
pub mod resolve;
pub mod source;
pub mod work;

pub use object::{parse_object, Object, PdfError};
pub use source::PdfSource;
pub use work::{ObjectRef, ObjectWork, WorkBudget};
