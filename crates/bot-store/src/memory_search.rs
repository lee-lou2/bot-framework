//! Hybrid memory search (Unicode trigram/token lexical + IEEE-754 f16 cosine + RRF + recency) and lock-free embedding backfill.
use crate::{
    codec::ids_ok,
    db::{R, Store},
    memory::{begin_read, flag_private, lineage, visibility_of},
};
use bot_core::{
    command::MemoryHit,
    id::{Attempt, MemoryId, ProfileId},
    memory::{
        MAX_EMBEDDING_DIMENSIONS, MAX_MEMORY_QUERY_BYTES, MemoryEmbedding, MemoryKind, MemoryQuery,
        UnembeddedMemory, dot_f16,
    },
    port::Error,
    state::Visibility,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{HashMap, HashSet};

const MAX_HITS: usize = 20;
const MAX_CODE_TOKENS: usize = 8;
const MAX_NORMAL_TOKENS: usize = 20;
const RRF_K: f64 = 30.0;
const RECENCY_HALF_DAYS: f64 = 30.0;
const RECENCY_WEIGHT: f64 = 0.002;
/// Empirically calibrated for normalized `gemini-embedding-2` 3072d (and compatible models) to
/// preserve recall while preventing unrelated queries from triggering false `private_memory_read` taint.
const DENSE_MIN_COS: f32 = 0.64;
const DENSE_MIN_Z: f32 = 2.0;
const CORE_MIN_COS: f32 = 0.62;
const CORE_MIN_Z: f32 = 1.8;
const HYBRID_LEX_COS_FLOOR: f32 = 0.58;
const CORE_LEX_COS_FLOOR: f32 = 0.54;
const MIN_Z_SAMPLE: usize = 6;

const KO_PARTICLES: &[&str] = &[
    "에서는",
    "으로는",
    "에게는",
    "한테서",
    "에서",
    "으로",
    "에게",
    "한테",
    "까지",
    "부터",
    "이랑",
    "은",
    "는",
    "이",
    "가",
    "을",
    "를",
    "의",
    "에",
    "로",
    "와",
    "과",
    "도",
    "만",
];

const STOPWORDS: &[&str] = &[
    "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be", "been",
    "before", "being", "but", "by", "can", "could", "did", "do", "does", "for", "from", "give",
    "had", "has", "have", "he", "her", "his", "how", "if", "in", "into", "is", "it", "its", "just",
    "may", "me", "might", "my", "no", "not", "of", "on", "or", "our", "over", "please", "she",
    "should", "show", "so", "some", "tell", "that", "the", "their", "them", "these", "they",
    "this", "those", "to", "us", "very", "was", "we", "were", "what", "when", "where", "which",
    "who", "why", "will", "with", "would", "yes", "you", "your",
];

fn is_hangul(c: char) -> bool {
    ('\u{ac00}'..='\u{d7a3}').contains(&c)
}

fn is_code_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | '#')
}

fn is_stopword(tok: &str) -> bool {
    STOPWORDS.binary_search(&tok).is_ok()
}

/// Splits a query into high-specificity identifier/code tokens and content word tokens.
pub(crate) fn extract_tokens(query: &str) -> (Vec<String>, Vec<String>) {
    let mut code_ids = Vec::new();
    let mut raw_normal = Vec::new();
    let mut chars = query.char_indices().peekable();
    while let Some(&(start, ch)) = chars.peek() {
        if is_code_char(ch) {
            let mut end = start;
            while let Some(&(idx, c)) = chars.peek() {
                if !is_code_char(c) {
                    break;
                }
                end = idx + c.len_utf8();
                chars.next();
            }
            let raw = query[start..end].trim_matches('.');
            if raw.chars().count() < 2 {
                continue;
            }
            let lower = raw.to_ascii_lowercase();
            let has_punct = raw.bytes().any(|b| matches!(b, b'_' | b'-' | b'/' | b'#'));
            let has_alnum = raw.bytes().any(|b| b.is_ascii_alphanumeric());
            let all_digits = raw.bytes().all(|b| b.is_ascii_digit());
            let mixed_upper_digit = raw.bytes().any(|b| b.is_ascii_uppercase())
                && raw.bytes().any(|b| b.is_ascii_digit());
            let is_code = (raw.len() >= 4 && has_punct && has_alnum)
                || (raw.len() >= 4 && all_digits)
                || (raw.len() >= 4 && mixed_upper_digit);
            if is_code {
                if code_ids.len() < MAX_CODE_TOKENS && !code_ids.contains(&lower) {
                    code_ids.push(lower);
                }
            } else if raw_normal.len() < MAX_NORMAL_TOKENS && !raw_normal.contains(&lower) {
                raw_normal.push(lower);
            }
        } else if ch.is_alphanumeric() {
            let mut end = start;
            while let Some(&(idx, c)) = chars.peek() {
                if !c.is_alphanumeric() || is_code_char(c) {
                    break;
                }
                end = idx + c.len_utf8();
                chars.next();
            }
            let lower = query[start..end].to_lowercase();
            if lower.chars().count() >= 2
                && raw_normal.len() < MAX_NORMAL_TOKENS
                && !raw_normal.contains(&lower)
            {
                raw_normal.push(lower);
            }
        } else {
            chars.next();
        }
    }
    let filtered: Vec<String> = raw_normal
        .iter()
        .filter(|t| !is_stopword(t))
        .cloned()
        .collect();
    let normal = if filtered.is_empty() && code_ids.is_empty() {
        raw_normal
    } else {
        filtered
    };
    (code_ids, normal)
}

/// Strips one trailing Korean particle from pure-Hangul tokens of 3+ syllables.
fn token_variants(tok: &str) -> Vec<String> {
    let mut out = vec![tok.to_owned()];
    if tok.chars().all(is_hangul) && tok.chars().count() >= 3 {
        for &suf in KO_PARTICLES {
            if let Some(stem) = tok.strip_suffix(suf)
                && stem.chars().count() >= 2
            {
                out.push(stem.to_owned());
                break;
            }
        }
    }
    out
}

/// Whole-word match for ASCII tokens (prevents `"in"` matching `"morning"` or `"scheduled"` matching `"schedule"`).
fn contains_token(body_lower: &str, token: &str) -> bool {
    if !token.is_ascii() {
        return body_lower.contains(token);
    }
    let bytes = body_lower.as_bytes();
    let len = token.len();
    let mut start = 0;
    while let Some(rel) = body_lower[start..].find(token) {
        let pos = start + rel;
        let end = pos + len;
        let left_ok = pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

fn push_word_trigrams(word: &str, out: &mut HashSet<[char; 3]>) {
    let mut buf = Vec::with_capacity(word.chars().count() + 3);
    buf.push(' ');
    buf.push(' ');
    buf.extend(word.chars());
    buf.push(' ');
    for win in buf.windows(3) {
        out.insert([win[0], win[1], win[2]]);
    }
}

fn query_trigrams(code_ids: &[String], variant_sets: &[Vec<String>]) -> HashSet<[char; 3]> {
    let mut out = HashSet::new();
    for code in code_ids {
        push_word_trigrams(code, &mut out);
    }
    for vars in variant_sets {
        if let Some(stem) = vars.last() {
            push_word_trigrams(stem, &mut out);
        }
    }
    out
}

/// Locale-independent Unicode character-trigram containment similarity in `[0.0, 1.0]`.
fn trigram_word_sim(q_trigrams: &HashSet<[char; 3]>, body_lower: &str) -> f32 {
    if q_trigrams.is_empty() {
        return 0.0;
    }
    let mut body_trigrams = HashSet::new();
    for word in body_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        push_word_trigrams(word, &mut body_trigrams);
    }
    let hits = q_trigrams
        .iter()
        .filter(|g| body_trigrams.contains(*g))
        .count();
    hits as f32 / q_trigrams.len() as f32
}

struct RowData {
    id: String,
    visibility: Visibility,
    kind: MemoryKind,
    body: String,
    updated_ms: i64,
    wsim: f32,
    cos: Option<f32>,
    code_hits: usize,
    norm_hits: usize,
    lex_score: f64,
}

fn rank_candidates(
    rows: Vec<RowData>,
    now_ms: i64,
    has_query_vec: bool,
    limit: usize,
) -> Vec<MemoryHit> {
    let dense_vals: Vec<f32> = rows.iter().filter_map(|r| r.cos).collect();
    let use_dense = has_query_vec && !dense_vals.is_empty();
    let (mean_c, std_c) = if use_dense {
        let n = dense_vals.len() as f64;
        let mean = dense_vals.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
        let var = dense_vals
            .iter()
            .map(|&v| {
                let d = f64::from(v) - mean;
                d * d
            })
            .sum::<f64>()
            / n;
        (mean as f32, (var.sqrt() as f32).max(1e-6))
    } else {
        (0.0, 1.0)
    };
    let use_z = dense_vals.len() >= MIN_Z_SAMPLE;

    let mut dense_order: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.cos.map(|_| i))
        .collect();
    dense_order.sort_by(|&a, &b| {
        rows[b]
            .cos
            .unwrap_or(-1.0)
            .total_cmp(&rows[a].cos.unwrap_or(-1.0))
            .then_with(|| rows[b].updated_ms.cmp(&rows[a].updated_ms))
            .then_with(|| rows[a].id.cmp(&rows[b].id))
    });
    let dense_rank: HashMap<usize, usize> = dense_order
        .into_iter()
        .enumerate()
        .map(|(rank, idx)| (idx, rank + 1))
        .collect();

    let mut lex_order: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            (r.code_hits > 0 || r.norm_hits > 0 || (use_dense && r.wsim >= 0.42)).then_some(i)
        })
        .collect();
    lex_order.sort_by(|&a, &b| {
        rows[b]
            .lex_score
            .total_cmp(&rows[a].lex_score)
            .then_with(|| rows[b].updated_ms.cmp(&rows[a].updated_ms))
            .then_with(|| rows[a].id.cmp(&rows[b].id))
    });
    let lex_rank: HashMap<usize, usize> = lex_order
        .into_iter()
        .enumerate()
        .map(|(rank, idx)| (idx, rank + 1))
        .collect();

    let mut scored: Vec<(usize, f64)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let exact_code = r.code_hits > 0;
        let kept = if use_dense {
            let cos = r.cos.unwrap_or(-1.0);
            let z = (cos - mean_c) / std_c;
            let dense_hit = cos >= DENSE_MIN_COS && (!use_z || z >= DENSE_MIN_Z);
            let lex_confirmed = ((r.norm_hits >= 2 || r.wsim >= 0.48)
                && cos >= HYBRID_LEX_COS_FLOOR)
                || (r.norm_hits >= 1 && cos >= DENSE_MIN_COS - 0.02 && (!use_z || z >= 1.5));
            let core_hit = r.kind == MemoryKind::Core
                && ((cos >= CORE_MIN_COS && (!use_z || z >= CORE_MIN_Z))
                    || (r.norm_hits >= 2 && cos >= CORE_LEX_COS_FLOOR));
            exact_code || dense_hit || lex_confirmed || core_hit
        } else {
            // Lexical-only fallback: stopwords are filtered and ASCII tokens match whole words,
            // so any content token or exact identifier hit is a real lexical match.
            exact_code || r.norm_hits >= 1
        };
        if !kept {
            continue;
        }
        let mut score = 0.0f64;
        if let Some(&rank) = dense_rank.get(&i) {
            score += 1.0 / (RRF_K + rank as f64);
        }
        if let Some(&rank) = lex_rank.get(&i) {
            let w = if exact_code { 1.5 } else { 0.9 };
            score += w / (RRF_K + rank as f64);
        }
        let age_days = ((now_ms - r.updated_ms).max(0) as f64) / 86_400_000.0;
        score += RECENCY_WEIGHT * (-age_days / RECENCY_HALF_DAYS).exp();
        scored.push((i, score));
    }

    scored.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| rows[b.0].updated_ms.cmp(&rows[a.0].updated_ms))
            .then_with(|| rows[a.0].id.cmp(&rows[b.0].id))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(i, _)| MemoryHit {
            id: MemoryId(rows[i].id.clone()),
            visibility: rows[i].visibility,
            text: rows[i].body.clone(),
        })
        .collect()
}

impl Store {
    pub(crate) async fn memory_search_hybrid(
        &self,
        at: &Attempt,
        query: &MemoryQuery,
    ) -> R<Vec<MemoryHit>> {
        if query.text.len() > MAX_MEMORY_QUERY_BYTES || query.text.contains('\0') {
            return Err(Error::Invalid.into());
        }
        let limit = usize::try_from(query.limit.clamp(1, MAX_HITS as u32)).unwrap_or(MAX_HITS);
        let trimmed = query.text.trim();
        if trimmed.is_empty() {
            let mut hits = self.memory_search(at, "").await?;
            hits.truncate(limit);
            return Ok(hits);
        }

        let (mut tx, me) = begin_read(self, at).await?;
        let (principal, kind, agent): (String, String, String) =
            (me.get("principal"), me.get("conv_kind"), me.get("agent"));
        let private_ok = matches!(kind.as_str(), "direct" | "cli");
        let now_ms: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;

        let (code_ids, normal) = extract_tokens(trimmed);
        let q_model = query.embedding.as_ref().map(|e| e.model.as_str());
        let q_dim = query.embedding.as_ref().map(|e| e.dim() as i32);

        let pg_rows = sqlx::query(
            "select m.id, m.visibility, m.kind, m.body, m.updated_ms,
                    case when $5::text is not null and m.embedding_model = $5 and m.embedding_dim = $6
                         then m.embedding_f16 else null end as emb
             from bot_memories m
             where m.profile = $1 and m.deleted_ms is null and m.superseded_by is null
               and m.agent = any($2)
               and (m.visibility = 'profile' or ($4 and m.owner = $3))",
        )
        .bind(&at.profile.0)
        .bind(lineage(&agent))
        .bind(&principal)
        .bind(private_ok)
        .bind(q_model)
        .bind(q_dim)
        .fetch_all(&mut *tx)
        .await?;

        let variant_sets: Vec<Vec<String>> = normal.iter().map(|t| token_variants(t)).collect();
        let q_trigrams = query_trigrams(&code_ids, &variant_sets);
        let mut rows = Vec::with_capacity(pg_rows.len());
        for row in pg_rows {
            let body: String = row.get("body");
            let body_lower = body.to_lowercase();
            let code_hits = code_ids
                .iter()
                .filter(|c| body_lower.contains(c.as_str()))
                .count();
            let norm_hits = variant_sets
                .iter()
                .filter(|vars| vars.iter().any(|v| contains_token(&body_lower, v)))
                .count();
            let wsim = trigram_word_sim(&q_trigrams, &body_lower);
            let emb: Option<Vec<u8>> = row.get("emb");
            let cos = match (&query.embedding, emb.as_deref()) {
                (Some(q), Some(bytes)) => dot_f16(bytes, &q.values),
                _ => None,
            };
            let lex_score = 3.0 * (code_hits as f64) + (norm_hits as f64) + 1.5 * f64::from(wsim);
            rows.push(RowData {
                id: row.get("id"),
                visibility: visibility_of(&row.get::<String, _>("visibility")),
                kind: MemoryKind::parse(&row.get::<String, _>("kind")).unwrap_or(MemoryKind::Fact),
                body,
                updated_ms: row.get("updated_ms"),
                wsim,
                cos,
                code_hits,
                norm_hits,
                lex_score,
            });
        }

        let hits = rank_candidates(rows, now_ms, query.embedding.is_some(), limit);
        if hits.iter().any(|h| h.visibility == Visibility::Private) {
            flag_private(&mut tx, at, &me.get::<String, _>("root")).await?;
        }
        tx.commit().await?;
        Ok(hits)
    }

    pub(crate) async fn memory_unembedded(
        &self,
        profile: &ProfileId,
        model: &str,
        dim: u32,
        limit: u32,
    ) -> R<Vec<UnembeddedMemory>> {
        ids_ok(&[&profile.0])?;
        if model.is_empty() || model.len() > 128 || !(1..=MAX_EMBEDDING_DIMENSIONS).contains(&dim) {
            return Err(Error::Invalid.into());
        }
        let rows = sqlx::query(
            "select id, body from bot_memories
             where profile = $1 and deleted_ms is null and superseded_by is null
               and (embedding_f16 is null or embedding_model is distinct from $2 or embedding_dim is distinct from $3)
             order by updated_ms desc, id
             limit $4",
        )
        .bind(&profile.0)
        .bind(model)
        .bind(dim as i32)
        .bind(i64::from(limit.clamp(1, 64)))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let body: String = r.get("body");
                let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
                UnembeddedMemory {
                    id: MemoryId(r.get("id")),
                    body,
                    body_sha256,
                }
            })
            .collect())
    }

    pub(crate) async fn memory_store_embedding(
        &self,
        profile: &ProfileId,
        id: &MemoryId,
        body_sha256: [u8; 32],
        embedding: &MemoryEmbedding,
    ) -> R<()> {
        ids_ok(&[&profile.0, &id.0])?;
        let Some(current): Option<String> = sqlx::query_scalar(
            "select body from bot_memories where profile = $1 and id = $2 and deleted_ms is null",
        )
        .bind(&profile.0)
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(());
        };
        let actual: [u8; 32] = Sha256::digest(current.as_bytes()).into();
        if actual != body_sha256 {
            return Ok(());
        }
        sqlx::query(
            "update bot_memories
             set embedding_model = $3, embedding_dim = $4, embedding_f16 = $5
             where profile = $1 and id = $2 and deleted_ms is null and body = $6",
        )
        .bind(&profile.0)
        .bind(&id.0)
        .bind(&embedding.model)
        .bind(embedding.dim() as i32)
        .bind(embedding.to_f16_bytes())
        .bind(&current)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tokens_splits_code_identifiers_and_korean_words() {
        let (codes, words) =
            extract_tokens("지난주에 발생한 ERR_LEASE_4092 티켓 PAY-8841 포트 55432 해결");
        assert_eq!(codes, ["err_lease_4092", "pay-8841", "55432"]);
        assert!(words.contains(&"지난주에".to_owned()));
        assert!(words.contains(&"티켓".to_owned()));
        assert_eq!(token_variants("지난주에"), ["지난주에", "지난주"]);
        assert_eq!(token_variants("클라이밍은"), ["클라이밍은", "클라이밍"]);
        assert_eq!(token_variants("scheduled"), ["scheduled"]);
    }

    #[test]
    fn stopwords_ascii_boundaries_and_unicode_trigrams_behave_cleanly() {
        assert!(STOPWORDS.windows(2).all(|w| w[0] < w[1]));
        let (codes, words) =
            extract_tokens("USER-QUESTION-9034 Tell me today's agenda and coffee plans");
        assert_eq!(codes, ["user-question-9034"]);
        assert!(words.contains(&"coffee".to_owned()));
        assert!(!words.contains(&"tell".to_owned()));
        assert!(!words.contains(&"and".to_owned()));

        assert!(contains_token("drink coffee only", "coffee"));
        assert!(!contains_token(
            "today's schedule has a meeting",
            "scheduled"
        ));
        assert!(!contains_token("morning coffee", "in"));
        assert!(contains_token("매주 수요일 클라이밍 짐", "클라이밍"));

        let q = query_trigrams(&[], &[vec!["클라이밍".into()]]);
        assert!(trigram_word_sim(&q, "매주 수요일 클라이밍 짐에 간다") > 0.9);
        assert_eq!(trigram_word_sim(&q, "오늘 서울 날씨"), 0.0);
    }
}
