#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitKind {
    FileBytes,
    Objects,
    PrevChain,
    Depth,
    RefChain,
    Tokens,
    ObjectBytes,
    DecodedBytes,
    TotalDecodedBytes,
    Pages,
    PageTreeDepth,
    ObjStmMembers,
    ContentOps,
    ImagePixels,
    ImageItems,
    KdfMemoryBytes,
    Jbig2Segments,
    Jbig2Symbols,
    CodecBytes,
    CodecWork,
}

impl LimitKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::FileBytes => "file_bytes",
            Self::Objects => "objects",
            Self::PrevChain => "prev_chain",
            Self::Depth => "depth",
            Self::RefChain => "ref_chain",
            Self::Tokens => "tokens",
            Self::ObjectBytes => "object_bytes",
            Self::DecodedBytes => "decoded_bytes",
            Self::TotalDecodedBytes => "total_decoded_bytes",
            Self::Pages => "pages",
            Self::PageTreeDepth => "page_tree_depth",
            Self::ObjStmMembers => "objstm_members",
            Self::ContentOps => "content_ops",
            Self::ImagePixels => "image_pixels",
            Self::ImageItems => "image_items",
            Self::KdfMemoryBytes => "kdf_memory_bytes",
            Self::Jbig2Segments => "jbig2_segments",
            Self::Jbig2Symbols => "jbig2_symbols",
            Self::CodecBytes => "codec_bytes",
            Self::CodecWork => "codec_work",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub limit: LimitKind,
    pub requested: u64,
    pub maximum: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_objects: u32,
    pub max_prev_chain: u32,
    pub max_depth: u32,
    pub max_ref_chain: u32,
    pub max_tokens_per_object: u32,
    pub max_object_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_total_decoded_bytes: u64,
    pub max_pages: u32,
    pub max_page_tree_depth: u32,
    pub max_objstm_members: u32,
    pub max_objstm_cache_bytes: u64,
    pub max_content_ops: u64,
    pub max_image_items: u32,
    pub max_image_cache_bytes: u64,
    pub max_kdf_memory_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1 << 30,
            max_objects: 8_000_000,
            max_prev_chain: 64,
            max_depth: 64,
            max_ref_chain: 32,
            max_tokens_per_object: 4_000_000,
            max_object_bytes: 64 << 20,
            max_decoded_bytes: 256 << 20,
            max_total_decoded_bytes: 4 << 30,
            max_pages: 1_000_000,
            max_page_tree_depth: 64,
            max_objstm_members: 1_000_000,
            max_objstm_cache_bytes: 64 << 20,
            max_content_ops: 50_000_000,
            max_image_items: 4096,
            max_image_cache_bytes: 64 << 20,
            max_kdf_memory_bytes: 64 << 20,
        }
    }
}

pub(crate) fn check(
    limit: LimitKind,
    requested: u64,
    maximum: u64,
) -> Result<(), crate::error::DocError> {
    if requested > maximum {
        Err(crate::error::DocError::Refused(Refusal {
            limit,
            requested,
            maximum,
        }))
    } else {
        Ok(())
    }
}
