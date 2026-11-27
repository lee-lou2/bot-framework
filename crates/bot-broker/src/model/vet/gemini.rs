//! Gemini GenerateContent request allowlist vetting for Antigravity CLI relay requests.

use super::{MAX_DEPTH, base64_ok, media_type_ok, tool_name_ok};
use bot_core::port::Error;
use serde_json::{Map, Value};

const BODY_FIELDS: [&str; 6] = [
    "contents",
    "generationConfig",
    "safetySettings",
    "systemInstruction",
    "toolConfig",
    "tools",
];

/// Provider-side cached context references can cross profile boundaries; rejected in both camelCase and snake_case.
const RESERVED: [&str; 2] = ["cachedContent", "cached_content"];

const DECL_FIELDS: [&str; 7] = [
    "name",
    "description",
    "parameters",
    "parametersJsonSchema",
    "response",
    "responseJsonSchema",
    "behavior",
];

const CONTENT_FIELDS: [&str; 2] = ["role", "parts"];

/// Strict part field allowlist (camelCase only) so ProtoJSON `snake_case` aliases (`file_data`, `executable_code`, etc.) cannot bypass vetting.
const PART_FIELDS: [&str; 6] = [
    "text",
    "thought",
    "thoughtSignature",
    "inlineData",
    "functionCall",
    "functionResponse",
];

const INLINE_DATA_FIELDS: [&str; 2] = ["mimeType", "data"];

const FUNCTION_RESPONSE_FIELDS: [&str; 6] = [
    "id",
    "name",
    "response",
    "willContinue",
    "scheduling",
    "parts",
];

const FUNCTION_CALLING_CONFIG_FIELDS: [&str; 2] = ["mode", "allowedFunctionNames"];

pub(super) fn vet_request(map: &Map<String, Value>, extra: &[String]) -> Result<(), Error> {
    if map.keys().any(|k| {
        RESERVED.contains(&k.as_str())
            || (!BODY_FIELDS.contains(&k.as_str()) && !extra.iter().any(|e| e == k))
    }) {
        return Err(Error::Invalid);
    }
    if let Some(tools) = map.get("tools") {
        vet_tools(tools)?;
    }
    if let Some(tool_cfg) = map.get("toolConfig") {
        vet_tool_config(tool_cfg)?;
    }
    if let Some(cfg) = map.get("generationConfig")
        && !cfg.is_object()
    {
        return Err(Error::Invalid);
    }
    if let Some(sys) = map.get("systemInstruction") {
        vet_content_obj(sys, 0)?;
    }
    if let Some(contents) = map.get("contents") {
        for entry in contents.as_array().ok_or(Error::Invalid)? {
            vet_content_obj(entry, 0)?;
        }
    }
    Ok(())
}

/// Accepts only `functionDeclarations` tools; blocks `googleSearch`, `codeExecution`, `urlContext`, etc.
fn vet_tools(tools: &Value) -> Result<(), Error> {
    for tool in tools.as_array().ok_or(Error::Invalid)? {
        let tool = tool.as_object().ok_or(Error::Invalid)?;
        if tool.len() != 1 || !tool.contains_key("functionDeclarations") {
            return Err(Error::Invalid);
        }
        let decls = tool
            .get("functionDeclarations")
            .and_then(Value::as_array)
            .ok_or(Error::Invalid)?;
        for decl in decls {
            let decl = decl.as_object().ok_or(Error::Invalid)?;
            let named = decl
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(tool_name_ok);
            if !named || decl.keys().any(|k| !DECL_FIELDS.contains(&k.as_str())) {
                return Err(Error::Invalid);
            }
        }
    }
    Ok(())
}

/// Permits only `functionCallingConfig` so server-side retrieval or grounding configs cannot be smuggled.
fn vet_tool_config(cfg: &Value) -> Result<(), Error> {
    let map = cfg.as_object().ok_or(Error::Invalid)?;
    if map.keys().any(|k| k != "functionCallingConfig") {
        return Err(Error::Invalid);
    }
    if let Some(fc) = map.get("functionCallingConfig") {
        let fc = fc.as_object().ok_or(Error::Invalid)?;
        if fc
            .keys()
            .any(|k| !FUNCTION_CALLING_CONFIG_FIELDS.contains(&k.as_str()))
        {
            return Err(Error::Invalid);
        }
        if let Some(mode) = fc.get("mode")
            && !mode.as_str().is_some_and(|m| {
                matches!(
                    m,
                    "MODE_UNSPECIFIED" | "AUTO" | "ANY" | "NONE" | "VALIDATED"
                )
            })
        {
            return Err(Error::Invalid);
        }
        if let Some(allowed) = fc.get("allowedFunctionNames") {
            for name in allowed.as_array().ok_or(Error::Invalid)? {
                if !name.as_str().is_some_and(tool_name_ok) {
                    return Err(Error::Invalid);
                }
            }
        }
    }
    Ok(())
}

fn vet_content_obj(entry: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid);
    }
    let obj = entry.as_object().ok_or(Error::Invalid)?;
    if obj.keys().any(|k| !CONTENT_FIELDS.contains(&k.as_str())) {
        return Err(Error::Invalid);
    }
    if let Some(parts) = obj.get("parts") {
        vet_parts(parts, depth)?;
    }
    Ok(())
}

fn vet_parts(parts: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid);
    }
    for part in parts.as_array().ok_or(Error::Invalid)? {
        let obj = part.as_object().ok_or(Error::Invalid)?;
        if obj.keys().any(|k| !PART_FIELDS.contains(&k.as_str())) {
            return Err(Error::Invalid);
        }
        if let Some(inline) = obj.get("inlineData") {
            vet_inline_data(inline)?;
        }
        if let Some(resp) = obj.get("functionResponse") {
            let resp = resp.as_object().ok_or(Error::Invalid)?;
            if resp
                .keys()
                .any(|k| !FUNCTION_RESPONSE_FIELDS.contains(&k.as_str()))
            {
                return Err(Error::Invalid);
            }
            if let Some(nested) = resp.get("parts") {
                vet_parts(nested, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn vet_inline_data(inline: &Value) -> Result<(), Error> {
    let map = inline.as_object().ok_or(Error::Invalid)?;
    if map
        .keys()
        .any(|k| !INLINE_DATA_FIELDS.contains(&k.as_str()))
    {
        return Err(Error::Invalid);
    }
    let ok_mime = media_type_ok(map.get("mimeType"));
    let ok_data = map
        .get("data")
        .and_then(Value::as_str)
        .is_some_and(base64_ok);
    if ok_mime && ok_data {
        Ok(())
    } else {
        Err(Error::Invalid)
    }
}
