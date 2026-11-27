//! Argument validation, canonical sealing, and preflight refusal checks for built-in memory tools.
use crate::{json::parse, ports::Sealed};
use bot_core::{
    builtin::{MEMORY_FORGET_TOOL, MEMORY_SAVE_TOOL, MEMORY_SEARCH_TOOL},
    cli::CliRefusal,
    command::JsonPayload,
    id::MemoryId,
    memory::{MAX_MEMORY_QUERY_BYTES, MAX_MEMORY_TEXT_BYTES, MemoryKind, forget_seal, save_seal},
    port::Error,
};
use serde_json::{Value, json};

pub(super) struct SearchArgs {
    pub query: String,
    pub limit: u32,
    pub sealed: Sealed,
}

pub(super) struct SaveArgs {
    pub text: String,
    pub kind: MemoryKind,
    pub supersedes: Option<MemoryId>,
    pub sealed: Sealed,
}

pub(in crate::service) fn judged(tool: &str, args: &JsonPayload) -> Option<CliRefusal> {
    let invalid = match tool {
        MEMORY_SEARCH_TOOL => parse_search(args).is_err(),
        MEMORY_SAVE_TOOL => parse_save(args).is_err(),
        MEMORY_FORGET_TOOL => parse_forget(args).is_err(),
        _ => false,
    };
    invalid.then_some(CliRefusal::Invalid)
}

fn id_str(raw: &str) -> Option<MemoryId> {
    (!raw.is_empty() && raw.len() <= 128 && raw.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| MemoryId(raw.to_owned()))
}

pub(super) fn parse_search(args: &JsonPayload) -> Result<SearchArgs, Error> {
    let Some(Value::Object(map)) = parse(&args.0) else {
        return Err(Error::Invalid);
    };
    if map.keys().any(|k| !matches!(k.as_str(), "query" | "limit")) {
        return Err(Error::Invalid);
    }
    let Some(query) = map.get("query").and_then(Value::as_str) else {
        return Err(Error::Invalid);
    };
    if query.trim().is_empty() || query.len() > MAX_MEMORY_QUERY_BYTES || query.contains('\0') {
        return Err(Error::Invalid);
    }
    let limit = match map.get("limit") {
        None => 10,
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=20).contains(n))
            .ok_or(Error::Invalid)?,
    };
    let canonical = bot_core::canon::canonicalize(&json!({ "limit": limit, "query": query }))
        .ok_or(Error::Invalid)?;
    let sha256 = bot_core::canon::sha256(&canonical);
    Ok(SearchArgs {
        query: query.to_owned(),
        limit,
        sealed: Sealed {
            canonical,
            sha256,
            destination: None,
        },
    })
}

pub(super) fn parse_save(args: &JsonPayload) -> Result<SaveArgs, Error> {
    let Some(Value::Object(map)) = parse(&args.0) else {
        return Err(Error::Invalid);
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "text" | "kind" | "supersedes"))
    {
        return Err(Error::Invalid);
    }
    let Some(text) = map.get("text").and_then(Value::as_str) else {
        return Err(Error::Invalid);
    };
    if text.trim().is_empty() || text.len() > MAX_MEMORY_TEXT_BYTES || text.contains('\0') {
        return Err(Error::Invalid);
    }
    let kind = match map.get("kind") {
        None => MemoryKind::Fact,
        Some(v) => v
            .as_str()
            .and_then(MemoryKind::parse)
            .ok_or(Error::Invalid)?,
    };
    let supersedes = match map.get("supersedes") {
        None => None,
        Some(v) => Some(v.as_str().and_then(id_str).ok_or(Error::Invalid)?),
    };
    let (canonical, sha256) = save_seal(text, kind, supersedes.as_ref()).ok_or(Error::Invalid)?;
    Ok(SaveArgs {
        text: text.to_owned(),
        kind,
        supersedes,
        sealed: Sealed {
            canonical,
            sha256,
            destination: None,
        },
    })
}

pub(super) fn parse_forget(args: &JsonPayload) -> Result<(MemoryId, Sealed), Error> {
    let Some(Value::Object(map)) = parse(&args.0) else {
        return Err(Error::Invalid);
    };
    if map.len() != 1 {
        return Err(Error::Invalid);
    }
    let id = map
        .get("id")
        .and_then(Value::as_str)
        .and_then(id_str)
        .ok_or(Error::Invalid)?;
    let (canonical, sha256) = forget_seal(&id).ok_or(Error::Invalid)?;
    Ok((
        id,
        Sealed {
            canonical,
            sha256,
            destination: None,
        },
    ))
}
