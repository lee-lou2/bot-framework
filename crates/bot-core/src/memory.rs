//! Memory v2 domain types, `Embedder` port, and IEEE-754 binary16 vector codec.
use crate::{
    id::{MemoryId, ProfileId},
    port::Reply,
};

/// Default embedding dimension (maximum output dimensionality for `gemini-embedding-2`).
pub const DEFAULT_EMBEDDING_DIMENSIONS: u32 = 3072;
/// Hard ceiling on stored embedding dimensions (`4096 * 2 = 8 KiB` per f16 vector).
pub const MAX_EMBEDDING_DIMENSIONS: u32 = 4096;
/// Maximum UTF-8 byte length of a stored memory body.
pub const MAX_MEMORY_TEXT_BYTES: usize = 8 * 1024;
/// Maximum UTF-8 byte length of a memory search query.
pub const MAX_MEMORY_QUERY_BYTES: usize = 1024;
/// Per-owner active non-seed memory quota within a single profile.
pub const MAX_ACTIVE_MEMORIES_PER_OWNER: i64 = 500;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MemoryKind {
    #[default]
    Fact,
    Core,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Core => "core",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fact" => Some(MemoryKind::Fact),
            "core" => Some(MemoryKind::Core),
            _ => None,
        }
    }
}

/// Validated, L2-normalized embedding vector tagged with its producing model name.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryEmbedding {
    pub model: String,
    pub values: Vec<f32>,
}

impl MemoryEmbedding {
    /// Validates finite values and dimension bounds, then L2-normalizes to unit length.
    pub fn normalize(model: impl Into<String>, mut values: Vec<f32>) -> Option<Self> {
        let model = model.into();
        let dim = u32::try_from(values.len()).ok()?;
        if model.is_empty()
            || model.len() > 128
            || model.contains('\0')
            || !(1..=MAX_EMBEDDING_DIMENSIONS).contains(&dim)
        {
            return None;
        }
        let mut sq = 0.0f64;
        for &v in &values {
            if !v.is_finite() {
                return None;
            }
            sq += f64::from(v) * f64::from(v);
        }
        let norm = sq.sqrt();
        if !norm.is_finite() || norm <= 1e-12 {
            return None;
        }
        let inv = (1.0 / norm) as f32;
        for v in &mut values {
            *v *= inv;
        }
        Some(Self { model, values })
    }

    pub fn dim(&self) -> u32 {
        self.values.len() as u32
    }

    pub fn to_f16_bytes(&self) -> Vec<u8> {
        encode_f16(&self.values)
    }
}

/// Parameters for hybrid memory lookup (`MemoryStore::search_hybrid`).
#[derive(Clone, Debug)]
pub struct MemoryQuery {
    pub text: String,
    pub embedding: Option<MemoryEmbedding>,
    pub limit: u32,
}

impl MemoryQuery {
    pub fn lexical(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            embedding: None,
            limit: 16,
        }
    }
}

/// Request payload for atomic `memory.save` persistence.
#[derive(Clone, Debug)]
pub struct MemorySave {
    pub text: String,
    pub kind: MemoryKind,
    pub supersedes: Option<MemoryId>,
    pub embedding: Option<MemoryEmbedding>,
}

/// Result of an atomic `memory.save` commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySaved {
    pub id: MemoryId,
    pub deduplicated: bool,
}

/// Active memory row missing an embedding for the configured model/dimension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnembeddedMemory {
    pub id: MemoryId,
    pub body: String,
    pub body_sha256: [u8; 32],
}

/// Canonical RFC 8785 JSON and SHA-256 shared by broker sealing and store verification for `memory.save`.
pub fn save_seal(
    text: &str,
    kind: MemoryKind,
    supersedes: Option<&MemoryId>,
) -> Option<(String, [u8; 32])> {
    let mut map = serde_json::Map::new();
    map.insert(
        "kind".into(),
        serde_json::Value::String(kind.as_str().into()),
    );
    if let Some(id) = supersedes {
        map.insert("supersedes".into(), serde_json::Value::String(id.0.clone()));
    }
    map.insert("text".into(), serde_json::Value::String(text.into()));
    let canonical = crate::canon::canonicalize(&serde_json::Value::Object(map))?;
    let sha = crate::canon::sha256(&canonical);
    Some((canonical, sha))
}

/// Canonical RFC 8785 JSON and SHA-256 shared by broker sealing and store verification for `memory.forget`.
pub fn forget_seal(id: &MemoryId) -> Option<(String, [u8; 32])> {
    let args = serde_json::json!({ "id": id.0 });
    let canonical = crate::canon::canonicalize(&args)?;
    let sha = crate::canon::sha256(&canonical);
    Some((canonical, sha))
}

/// Host embedding provider port (OpenAI-compatible `/embeddings` API or local loopback server).
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    fn dimensions(&self) -> u32;
    fn embed<'a>(
        &'a self,
        profile: &'a ProfileId,
        text: &'a str,
    ) -> Reply<'a, Option<MemoryEmbedding>>;
}

/// Existing private memory passed to a System One decision model for supersede/forget matching.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryCandidate {
    pub id: MemoryId,
    pub text: String,
}

/// One memory item extracted for automatic persistence by a decision model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryDecisionItem {
    pub text: String,
    pub kind: MemoryKind,
}

/// Typed outcome returned by a System One memory judge (`d1.2-ultra`, `jev-latest`, etc.).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryDecision {
    None,
    Save {
        items: Vec<MemoryDecisionItem>,
        supersedes: Option<MemoryId>,
    },
    Forget(MemoryId),
}

/// Host System One decision port classifying whether a 1:1 user turn should save, supersede, or forget memory.
pub trait MemoryJudge: Send + Sync {
    fn model(&self) -> &str;
    fn judge<'a>(
        &'a self,
        profile: &'a ProfileId,
        user_message: &'a str,
        existing: &'a [MemoryCandidate],
    ) -> Reply<'a, MemoryDecision>;
}

/// Encodes `f32` values into little-endian IEEE-754 binary16 bytes.
pub fn encode_f16(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 2);
    for &v in values {
        out.extend_from_slice(&f32_to_f16(v).to_le_bytes());
    }
    out
}

/// Decodes little-endian IEEE-754 binary16 bytes into `f32` values.
pub fn decode_f16(bytes: &[u8]) -> Option<Vec<f32>> {
    let (chunks, rem) = bytes.as_chunks::<2>();
    if !rem.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(chunks.len());
    for &chunk in chunks {
        out.push(f16_to_f32(u16::from_le_bytes(chunk)));
    }
    Some(out)
}

/// Zero-allocation dot product between an f16 little-endian byte buffer and an f32 query vector.
pub fn dot_f16(bytes: &[u8], query: &[f32]) -> Option<f32> {
    let (chunks, rem) = bytes.as_chunks::<2>();
    if !rem.is_empty() || chunks.len() != query.len() {
        return None;
    }
    let mut sum = 0.0f32;
    for (&chunk, &q) in chunks.iter().zip(query.iter()) {
        sum += f16_to_f32(u16::from_le_bytes(chunk)) * q;
    }
    Some(sum)
}

/// IEEE-754 round-to-nearest-even conversion from `f32` to binary16 bits.
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;
    if exp == 0xff {
        return if mant == 0 {
            sign | 0x7c00
        } else {
            sign | 0x7e00 | ((mant >> 13) as u16)
        };
    }
    let unbiased = exp - 127 + 15;
    if unbiased >= 31 {
        return sign | 0x7c00;
    }
    if unbiased <= 0 {
        if unbiased < -10 {
            return sign;
        }
        let m = (mant | 0x0080_0000) >> (1 - unbiased);
        let half = 0x1000u32;
        let round = if (m & 0x1fff) > half || ((m & 0x1fff) == half && (m & 0x2000) != 0) {
            1
        } else {
            0
        };
        return sign | ((m >> 13) as u16 + round);
    }
    let half = 0x1000u32;
    let round = if (mant & 0x1fff) > half || ((mant & 0x1fff) == half && (mant & 0x2000) != 0) {
        1
    } else {
        0
    };
    let h = ((unbiased as u32) << 10) + (mant >> 13) + round;
    sign | (h as u16)
}

/// Exact IEEE-754 conversion from binary16 bits to `f32`.
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = u32::from((bits >> 10) & 0x1f);
    let mant = u32::from(bits & 0x03ff);
    let fbits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            let mut m = mant;
            let mut e = 127 - 14;
            while (m & 0x0400) == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x03ff) << 13)
        }
    } else if exp == 31 {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + (127 - 15)) << 23) | (mant << 13)
    };
    f32::from_bits(fbits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_codec_round_trips_unit_vectors_at_max_dimension() {
        let dim = DEFAULT_EMBEDDING_DIMENSIONS as usize;
        let raw_a: Vec<f32> = (0..dim).map(|i| ((i as f32 * 0.37).sin()) * 0.05).collect();
        let raw_b: Vec<f32> = (0..dim)
            .map(|i| ((i as f32 * 0.37 + 0.4).sin()) * 0.05)
            .collect();
        let a = MemoryEmbedding::normalize("gemini-embedding-2", raw_a).unwrap();
        let b = MemoryEmbedding::normalize("gemini-embedding-2", raw_b).unwrap();
        assert_eq!(a.dim(), DEFAULT_EMBEDDING_DIMENSIONS);

        let bytes = a.to_f16_bytes();
        assert_eq!(bytes.len(), dim * 2);
        let decoded = decode_f16(&bytes).unwrap();
        assert_eq!(decoded.len(), dim);

        let exact: f32 = a.values.iter().zip(&b.values).map(|(x, y)| x * y).sum();
        let approx = dot_f16(&bytes, &b.values).unwrap();
        assert!(
            (exact - approx).abs() < 5e-4,
            "exact={exact} approx={approx}"
        );
        assert!(dot_f16(&bytes[..10], &b.values).is_none());
        assert!(decode_f16(&[1, 2, 3]).is_none());
    }

    #[test]
    fn f16_handles_special_and_subnormal_values() {
        for v in [0.0f32, -0.0, 1.0, -1.0, 0.5, -0.0001, 1e-5, 1e-8] {
            let back = f16_to_f32(f32_to_f16(v));
            assert!((v - back).abs() < 1e-3, "v={v} back={back}");
        }
        assert!(f16_to_f32(f32_to_f16(f32::INFINITY)).is_infinite());
        assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
    }

    #[test]
    fn normalize_rejects_empty_non_finite_zero_and_oversized() {
        assert!(MemoryEmbedding::normalize("m", vec![]).is_none());
        assert!(MemoryEmbedding::normalize("", vec![1.0]).is_none());
        assert!(MemoryEmbedding::normalize("m", vec![0.0, 0.0]).is_none());
        assert!(MemoryEmbedding::normalize("m", vec![f32::NAN]).is_none());
        assert!(MemoryEmbedding::normalize("m", vec![f32::INFINITY]).is_none());
        assert!(
            MemoryEmbedding::normalize("m", vec![1.0; (MAX_EMBEDDING_DIMENSIONS + 1) as usize])
                .is_none()
        );
        let unit = MemoryEmbedding::normalize("m", vec![3.0, 4.0]).unwrap();
        assert!((unit.values[0] - 0.6).abs() < 1e-6);
        assert!((unit.values[1] - 0.8).abs() < 1e-6);
    }
}
