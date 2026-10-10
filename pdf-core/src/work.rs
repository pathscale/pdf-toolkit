use core::fmt;
use core::ops::Range;

use crate::source::{PdfSource, RawSpan, SourceError};

/// Identifies a PDF indirect object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectRef {
    number: u32,
    generation: u16,
}

impl ObjectRef {
    pub const fn new(number: u32, generation: u16) -> Self {
        Self { number, generation }
    }

    pub const fn number(self) -> u32 {
        self.number
    }

    pub const fn generation(self) -> u16 {
        self.generation
    }
}

/// Identifies a page by its index and indirect object reference.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PageId {
    index: usize,
    object: ObjectRef,
}

impl PageId {
    pub const fn new(index: usize, object: ObjectRef) -> Self {
        Self { index, object }
    }

    pub const fn index(self) -> usize {
        self.index
    }

    pub const fn object(self) -> ObjectRef {
        self.object
    }
}

/// Identifies the raw bytes belonging to a stream object.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StreamRef {
    owner: ObjectRef,
    raw_range: Range<usize>,
}

impl StreamRef {
    pub fn new(owner: ObjectRef, raw_range: Range<usize>) -> Self {
        Self { owner, raw_range }
    }

    pub const fn owner(&self) -> ObjectRef {
        self.owner
    }

    pub fn raw_range(&self) -> Range<usize> {
        self.raw_range.clone()
    }
}

/// Identifies a candidate object byte range in a PDF source.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ObjectWork {
    id: ObjectRef,
    candidate_range: Range<usize>,
}

impl ObjectWork {
    pub fn new(id: ObjectRef, candidate_range: Range<usize>) -> Self {
        Self { id, candidate_range }
    }

    pub const fn id(&self) -> ObjectRef {
        self.id
    }

    pub fn candidate_range(&self) -> Range<usize> {
        self.candidate_range.clone()
    }

    /// Returns the checked candidate bytes borrowed from `source`.
    pub fn span<'source>(
        &self,
        source: &'source PdfSource,
        budget: &WorkBudget,
    ) -> Result<RawSpan<'source>, WorkError> {
        let start = self.candidate_range.start;
        let end = self.candidate_range.end;
        let length = end
            .checked_sub(start)
            .ok_or(WorkError::InvertedRange { start, end })?;

        if source
            .bytes()
            .get(self.candidate_range.clone())
            .is_none()
        {
            return Err(WorkError::RangeOutOfBounds {
                start,
                end,
                source_len: source.bytes().len(),
            });
        }

        budget.check_input_bytes(length)?;
        source
            .span(self.candidate_range.clone())
            .map_err(WorkError::from)
    }
}

/// The resource limit checked by a work budget.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WorkLimit {
    InputBytes,
    Tokens,
    Depth,
    OutputBytes,
    Operators,
}

impl fmt::Display for WorkLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputBytes => formatter.write_str("input bytes"),
            Self::Tokens => formatter.write_str("tokens"),
            Self::Depth => formatter.write_str("depth"),
            Self::OutputBytes => formatter.write_str("output bytes"),
            Self::Operators => formatter.write_str("operators"),
        }
    }
}

/// Caps the resources available to work performed on a PDF source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkBudget {
    max_input_bytes: usize,
    max_tokens: usize,
    max_depth: usize,
    max_output_bytes: usize,
    max_operators: usize,
}

impl WorkBudget {
    pub const fn new(
        max_input_bytes: usize,
        max_tokens: usize,
        max_depth: usize,
        max_output_bytes: usize,
        max_operators: usize,
    ) -> Self {
        Self {
            max_input_bytes,
            max_tokens,
            max_depth,
            max_output_bytes,
            max_operators,
        }
    }

    pub const fn max_input_bytes(self) -> usize {
        self.max_input_bytes
    }

    pub const fn max_tokens(self) -> usize {
        self.max_tokens
    }

    pub const fn max_depth(self) -> usize {
        self.max_depth
    }

    pub const fn max_output_bytes(self) -> usize {
        self.max_output_bytes
    }

    pub const fn max_operators(self) -> usize {
        self.max_operators
    }

    pub fn check_input_bytes(&self, requested: usize) -> Result<(), WorkError> {
        Self::check_limit(WorkLimit::InputBytes, requested, self.max_input_bytes)
    }

    pub fn check_tokens(&self, requested: usize) -> Result<(), WorkError> {
        Self::check_limit(WorkLimit::Tokens, requested, self.max_tokens)
    }

    pub fn check_depth(&self, requested: usize) -> Result<(), WorkError> {
        Self::check_limit(WorkLimit::Depth, requested, self.max_depth)
    }

    pub fn check_output_bytes(&self, requested: usize) -> Result<(), WorkError> {
        Self::check_limit(WorkLimit::OutputBytes, requested, self.max_output_bytes)
    }

    pub fn check_operators(&self, requested: usize) -> Result<(), WorkError> {
        Self::check_limit(WorkLimit::Operators, requested, self.max_operators)
    }

    fn check_limit(
        limit: WorkLimit,
        requested: usize,
        maximum: usize,
    ) -> Result<(), WorkError> {
        if requested > maximum {
            Err(WorkError::BudgetExceeded {
                limit,
                requested,
                maximum,
            })
        } else {
            Ok(())
        }
    }
}

/// An error returned when a work value fails validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkError {
    InvertedRange {
        start: usize,
        end: usize,
    },
    RangeOutOfBounds {
        start: usize,
        end: usize,
        source_len: usize,
    },
    BudgetExceeded {
        limit: WorkLimit,
        requested: usize,
        maximum: usize,
    },
    Source(SourceError),
}

impl fmt::Display for WorkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvertedRange { start, end } => {
                write!(formatter, "work range {start}..{end} is inverted")
            }
            Self::RangeOutOfBounds {
                start,
                end,
                source_len,
            } => write!(
                formatter,
                "work range {start}..{end} is outside {source_len} source bytes"
            ),
            Self::BudgetExceeded {
                limit,
                requested,
                maximum,
            } => write!(
                formatter,
                "work requests {requested} {limit}, exceeding the limit of {maximum}"
            ),
            Self::Source(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl From<SourceError> for WorkError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WorkError {}
