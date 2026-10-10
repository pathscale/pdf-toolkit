use alloc::sync::Arc;
use core::fmt;
use core::ops::Range;

const HEADER_PREFIX: &[u8; 5] = b"%PDF-";
const HEADER_LENGTH: usize = 8;
const HEADER_SEARCH_LIMIT: usize = 1024;

/// A parsed PDF file header.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PdfHeader {
    offset: usize,
    version: PdfVersion,
}

impl PdfHeader {
    /// Returns the byte offset where the header begins.
    pub const fn offset(self) -> usize {
        self.offset
    }

    /// Returns the version written in the header.
    pub const fn version(self) -> PdfVersion {
        self.version
    }
}

/// A numeric PDF version from the file header.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PdfVersion {
    major: u8,
    minor: u8,
}

impl PdfVersion {
    pub const fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }

    pub const fn major(self) -> u8 {
        self.major
    }

    pub const fn minor(self) -> u8 {
        self.minor
    }
}

impl fmt::Display for PdfVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// A stable SHA-256 digest of the exact bytes held by a source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceDigest([u8; 32]);

impl SourceDigest {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Identifies one immutable source revision by its content and revision number.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceIdentity {
    digest: SourceDigest,
    revision: u64,
}

impl SourceIdentity {
    pub const fn digest(self) -> SourceDigest {
        self.digest
    }

    pub const fn revision(self) -> u64 {
        self.revision
    }
}

/// Errors returned while creating a source or checking one of its byte ranges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceError {
    TruncatedHeader {
        offset: usize,
        available: usize,
    },
    MissingHeader,
    InvalidHeaderVersion {
        offset: usize,
    },
    SliceOutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader { offset, available } => write!(
                formatter,
                "PDF header at byte {offset} is truncated with {available} bytes available"
            ),
            Self::MissingHeader => formatter.write_str("PDF header was not found"),
            Self::InvalidHeaderVersion { offset } => {
                write!(formatter, "PDF header at byte {offset} has an invalid version")
            }
            Self::SliceOutOfBounds { start, end, len } => write!(
                formatter,
                "source range {start}..{end} is outside {len} bytes"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SourceError {}

/// An immutable owner of PDF source bytes and their parsed header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PdfSource {
    bytes: Arc<[u8]>,
    header: PdfHeader,
    identity: SourceIdentity,
}

impl PdfSource {
    /// Creates a source at revision zero from owned or shared bytes.
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Result<Self, SourceError> {
        Self::with_revision(bytes, 0)
    }

    /// Creates a source with a caller-supplied revision number.
    pub fn with_revision(
        bytes: impl Into<Arc<[u8]>>,
        revision: u64,
    ) -> Result<Self, SourceError> {
        let bytes = bytes.into();
        let header = parse_header(&bytes)?;
        let identity = SourceIdentity {
            digest: sha256(&bytes),
            revision,
        };

        Ok(Self {
            bytes,
            header,
            identity,
        })
    }

    /// Returns the complete source byte slice.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the parsed header and version.
    pub const fn header(&self) -> PdfHeader {
        self.header
    }

    /// Returns the stable content digest and source revision.
    pub const fn identity(&self) -> SourceIdentity {
        self.identity
    }

    /// Returns a checked borrowed slice of the source bytes.
    pub fn slice(&self, range: Range<usize>) -> Result<&[u8], SourceError> {
        self.bytes
            .get(range.clone())
            .ok_or(SourceError::SliceOutOfBounds {
                start: range.start,
                end: range.end,
                len: self.bytes.len(),
            })
    }

    /// Creates a checked span that borrows its bytes from this source.
    pub fn span(&self, range: Range<usize>) -> Result<RawSpan<'_>, SourceError> {
        let bytes = self.slice(range.clone())?;
        Ok(RawSpan { bytes, range })
    }
}

/// A byte range that borrows directly from a source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSpan<'source> {
    bytes: &'source [u8],
    range: Range<usize>,
}

impl<'source> RawSpan<'source> {
    /// Returns the borrowed bytes in this span.
    pub const fn as_bytes(&self) -> &'source [u8] {
        self.bytes
    }

    /// Returns the span's range in its source.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }
}

fn parse_header(bytes: &[u8]) -> Result<PdfHeader, SourceError> {
    let search_len = bytes
        .len()
        .min(HEADER_SEARCH_LIMIT + HEADER_PREFIX.len() - 1);
    let header_offset = bytes[..search_len]
        .windows(HEADER_PREFIX.len())
        .position(|window| window == HEADER_PREFIX);

    let Some(offset) = header_offset else {
        if bytes.len() < HEADER_LENGTH {
            return Err(SourceError::TruncatedHeader {
                offset: 0,
                available: bytes.len(),
            });
        }
        return Err(SourceError::MissingHeader);
    };

    let available = bytes.len() - offset;
    if available < HEADER_LENGTH {
        return Err(SourceError::TruncatedHeader { offset, available });
    }

    let major = bytes[offset + 5];
    let separator = bytes[offset + 6];
    let minor = bytes[offset + 7];
    if !major.is_ascii_digit() || separator != b'.' || !minor.is_ascii_digit() {
        return Err(SourceError::InvalidHeaderVersion { offset });
    }

    Ok(PdfHeader {
        offset,
        version: PdfVersion::new(major - b'0', minor - b'0'),
    })
}

fn sha256(bytes: &[u8]) -> SourceDigest {
    const INITIAL_STATE: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND_CONSTANTS: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut state = INITIAL_STATE;
    let complete_length = bytes.len() - bytes.len() % 64;
    for block in bytes[..complete_length].chunks_exact(64) {
        compress_block(&mut state, block, &ROUND_CONSTANTS);
    }

    let remainder = &bytes[complete_length..];
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let padded_length = if remainder.len() < 56 { 64 } else { 128 };
    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    tail[padded_length - 8..padded_length].copy_from_slice(&bit_length.to_be_bytes());
    for block in tail[..padded_length].chunks_exact(64) {
        compress_block(&mut state, block, &ROUND_CONSTANTS);
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        let start = index * 4;
        digest[start..start + 4].copy_from_slice(&word.to_be_bytes());
    }
    SourceDigest(digest)
}

fn compress_block(state: &mut [u32; 8], block: &[u8], round_constants: &[u32; 64]) {
    let mut schedule = [0u32; 64];
    for (index, word) in block.chunks_exact(4).enumerate() {
        schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for index in 16..64 {
        let x = schedule[index - 15];
        let small_sigma_0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let y = schedule[index - 2];
        let small_sigma_1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(small_sigma_0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(small_sigma_1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let big_sigma_1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let first = h
            .wrapping_add(big_sigma_1)
            .wrapping_add(choose)
            .wrapping_add(round_constants[index])
            .wrapping_add(schedule[index]);
        let big_sigma_0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let second = big_sigma_0.wrapping_add(majority);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(first);
        d = c;
        c = b;
        b = a;
        a = first.wrapping_add(second);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}
