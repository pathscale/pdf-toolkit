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

#[cfg(test)]
mod tests {
    use super::{ObjectRef, ObjectWork, WorkBudget, WorkError, WorkLimit};
    use crate::source::PdfSource;
    use alloc::sync::Arc;

    fn inline_source() -> PdfSource {
        PdfSource::new(Arc::<[u8]>::from(
            &b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n"[..],
        ))
        .unwrap()
    }

    #[test]
    fn object_work_borrows_exact_source_range() {
        let source = inline_source();
        let range = 9..16;
        let work = ObjectWork::new(ObjectRef::new(1, 0), range.clone());
        let budget = WorkBudget::new(7, 10, 4, 32, 8);

        let span = work.span(&source, &budget).unwrap();

        assert_eq!(span.as_bytes(), b"1 0 obj");
        assert_eq!(span.range(), range);
        assert_eq!(
            span.as_bytes().as_ptr(),
            source.bytes().get(range).unwrap().as_ptr()
        );
    }

    #[test]
    fn object_work_refuses_inverted_range() {
        let source = inline_source();
        let work = ObjectWork::new(ObjectRef::new(1, 0), 16..9);
        let budget = WorkBudget::new(32, 10, 4, 32, 8);

        assert!(matches!(
            work.span(&source, &budget),
            Err(WorkError::InvertedRange { start: 16, end: 9 })
        ));
    }

    #[test]
    fn object_work_refuses_over_budget_range() {
        let source = inline_source();
        let work = ObjectWork::new(ObjectRef::new(1, 0), 9..16);
        let budget = WorkBudget::new(6, 10, 4, 32, 8);

        assert!(matches!(
            work.span(&source, &budget),
            Err(WorkError::BudgetExceeded {
                limit: WorkLimit::InputBytes,
                requested: 7,
                maximum: 6,
            })
        ));
    }

    #[test]
    fn same_source_supports_independent_work_values() {
        let source = PdfSource::new(Arc::<[u8]>::from(&b"%PDF-1.4\none two"[..])).unwrap();
        let first_range = 9..12;
        let second_range = 13..16;
        let first = ObjectWork::new(ObjectRef::new(1, 0), first_range.clone());
        let second = ObjectWork::new(ObjectRef::new(2, 0), second_range.clone());
        let budget = WorkBudget::new(6, 10, 4, 32, 8);

        let first_span = first.span(&source, &budget).unwrap();
        let second_span = second.span(&source, &budget).unwrap();

        assert_eq!(first_span.as_bytes(), b"one");
        assert_eq!(first_span.range(), first_range);
        assert_eq!(second_span.as_bytes(), b"two");
        assert_eq!(second_span.range(), second_range);
        assert!(first_span.range().end <= second_span.range().start);
        assert_eq!(
            first_span.as_bytes().as_ptr(),
            source.bytes().get(first_range).unwrap().as_ptr()
        );
        assert_eq!(
            second_span.as_bytes().as_ptr(),
            source.bytes().get(second_range).unwrap().as_ptr()
        );
    }

    #[test]
    fn object_work_refuses_out_of_source_range() {
        let source = inline_source();
        let end = source.bytes().len() + 1;
        let work = ObjectWork::new(ObjectRef::new(1, 0), 0..end);
        let budget = WorkBudget::new(end, 10, 4, 32, 8);

        assert!(matches!(
            work.span(&source, &budget),
            Err(WorkError::RangeOutOfBounds {
                start: 0,
                end: range_end,
                source_len,
            }) if range_end == end && source_len == source.bytes().len()
        ));
    }

    #[test]
    fn work_budget_checks_each_resource_cap() {
        let budget = WorkBudget::new(1, 2, 3, 4, 5);

        assert_eq!(budget.max_input_bytes(), 1);
        assert_eq!(budget.max_tokens(), 2);
        assert_eq!(budget.max_depth(), 3);
        assert_eq!(budget.max_output_bytes(), 4);
        assert_eq!(budget.max_operators(), 5);
        assert_eq!(budget.check_input_bytes(1), Ok(()));
        assert_eq!(budget.check_tokens(2), Ok(()));
        assert_eq!(budget.check_depth(3), Ok(()));
        assert_eq!(budget.check_output_bytes(4), Ok(()));
        assert_eq!(budget.check_operators(5), Ok(()));
        assert!(matches!(
            budget.check_input_bytes(2),
            Err(WorkError::BudgetExceeded {
                limit: WorkLimit::InputBytes,
                requested: 2,
                maximum: 1,
            })
        ));
        assert!(matches!(
            budget.check_tokens(3),
            Err(WorkError::BudgetExceeded {
                limit: WorkLimit::Tokens,
                requested: 3,
                maximum: 2,
            })
        ));
        assert!(matches!(
            budget.check_depth(4),
            Err(WorkError::BudgetExceeded {
                limit: WorkLimit::Depth,
                requested: 4,
                maximum: 3,
            })
        ));
        assert!(matches!(
            budget.check_output_bytes(5),
            Err(WorkError::BudgetExceeded {
                limit: WorkLimit::OutputBytes,
                requested: 5,
                maximum: 4,
            })
        ));
        assert!(matches!(
            budget.check_operators(6),
            Err(WorkError::BudgetExceeded {
                limit: WorkLimit::Operators,
                requested: 6,
                maximum: 5,
            })
        ));
    }
}
