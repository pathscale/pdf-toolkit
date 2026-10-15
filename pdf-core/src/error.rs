use core::fmt;

use crate::filter::FilterError;
use crate::limits::{LimitKind, Refusal};
use crate::source::SourceError;
use crate::work::ObjectRef;

#[derive(Clone, Debug, PartialEq)]
pub enum DocError {
    Source(SourceError),
    Refused(Refusal),
    Encrypted,
    Unsupported {
        feature: &'static str,
    },
    Malformed {
        offset: u64,
        what: &'static str,
    },
    MissingObject {
        id: ObjectRef,
    },
    WrongType {
        id: Option<ObjectRef>,
        expected: &'static str,
    },
    Filter(FilterError),
}

impl DocError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Source(_) => "source",
            Self::Refused(refusal) => match refusal.limit {
                LimitKind::FileBytes => "refused:file_bytes",
                LimitKind::Objects => "refused:objects",
                LimitKind::PrevChain => "refused:prev_chain",
                LimitKind::Depth => "refused:depth",
                LimitKind::RefChain => "refused:ref_chain",
                LimitKind::Tokens => "refused:tokens",
                LimitKind::ObjectBytes => "refused:object_bytes",
                LimitKind::DecodedBytes => "refused:decoded_bytes",
                LimitKind::TotalDecodedBytes => "refused:total_decoded_bytes",
                LimitKind::Pages => "refused:pages",
                LimitKind::PageTreeDepth => "refused:page_tree_depth",
                LimitKind::ObjStmMembers => "refused:objstm_members",
                LimitKind::ContentOps => "refused:content_ops",
                LimitKind::ImagePixels => "refused:image_pixels",
                LimitKind::ImageItems => "refused:image_items",
                LimitKind::KdfMemoryBytes => "refused:kdf_memory_bytes",
                LimitKind::Jbig2Segments => "refused:jbig2_segments",
                LimitKind::Jbig2Symbols => "refused:jbig2_symbols",
                LimitKind::CodecBytes => "refused:codec_bytes",
                LimitKind::CodecWork => "refused:codec_work",
            },
            Self::Encrypted => "encrypted",
            Self::Unsupported { .. } => "unsupported",
            Self::Malformed { .. } => "malformed",
            Self::MissingObject { .. } => "missing_object",
            Self::WrongType { .. } => "wrong_type",
            Self::Filter(error) => match error {
                FilterError::OutputLimit { .. } => "filter:output_limit",
                FilterError::Refused(_) => "filter:refused",
                FilterError::Unsupported { .. } => "filter:unsupported",
                FilterError::BadParams { .. } => "filter:bad_params",
                FilterError::Corrupt { .. } => "filter:corrupt",
            },
        }
    }
}

impl fmt::Display for DocError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => fmt::Display::fmt(error, formatter),
            Self::Refused(refusal) => write!(
                formatter,
                "{} requested {}, maximum {}",
                refusal.limit.code(),
                refusal.requested,
                refusal.maximum
            ),
            Self::Encrypted => formatter.write_str("encrypted PDF stream"),
            Self::Unsupported { feature } => {
                write!(formatter, "unsupported PDF feature: {feature}")
            }
            Self::Malformed { offset, what } => {
                write!(formatter, "malformed {what} at byte {offset}")
            }
            Self::MissingObject { id } => write!(
                formatter,
                "missing object {} {}",
                id.number(),
                id.generation()
            ),
            Self::WrongType { id, expected } => {
                write!(formatter, "expected {expected} in object {id:?}")
            }
            Self::Filter(error) => write!(formatter, "PDF filter error: {error:?}"),
        }
    }
}

impl From<SourceError> for DocError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

impl From<Refusal> for DocError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<FilterError> for DocError {
    fn from(error: FilterError) -> Self {
        match error {
            FilterError::OutputLimit { limit } => Self::Refused(Refusal {
                limit: LimitKind::DecodedBytes,
                requested: limit.saturating_add(1),
                maximum: limit,
            }),
            FilterError::Refused(refusal) => Self::Refused(refusal),
            error => Self::Filter(error),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DocError {}

#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub offset: Option<u64>,
    pub object: Option<ObjectRef>,
    pub what: &'static str,
}

pub(crate) fn malformed(offset: usize, what: &'static str) -> DocError {
    DocError::Malformed {
        offset: offset as u64,
        what,
    }
}
