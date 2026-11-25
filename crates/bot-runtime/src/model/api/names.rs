//! 도구 이름 별칭표. 상류 API는 도구 이름을 `^[a-zA-Z0-9_-]{1,64}$`로 제한하지만 도구 ID는 `agenda.list`처럼 점을 쓴다. 요청마다 도구 ID ↔ 별칭을 **일대일로 되돌릴 수 있는** 표를
//! 만들어 모델에게는 별칭만 보이고, 응답의 이름은 이 표로 정확히 도구 ID로 되돌린다. 표에 없는 이름(모델이 지어낸 것)은 도구를 부르지 않는다.
//!
//! 이미 규칙에 맞는 ID는 그대로가 별칭이다. 맞지 않는 ID는 글자를 `_`로 바꾼 앞부분 + `_` + ID의 SHA-256 앞 8자리로 만든다(`a.b`와 `a_b`가 같은 별칭이 되지 않게). 같은 입력이면 항상 같은
//! 표이고(ID는 정렬해 처리한다) 별칭이 겹치면 해시 입력에 차례 번호를 더해 다시 만든다.

use bot_core::id::ToolId;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// 상류가 받는 이름의 최대 길이
const MAX_NAME: usize = 64;
/// 별칭 끝에 붙이는 `_` + 해시 8자리
const SUFFIX: usize = 9;

#[derive(Debug)]
pub(crate) struct NameTable {
    alias_of: BTreeMap<String, String>,
    tool_of: BTreeMap<String, ToolId>,
}

/// 상류 규칙에 맞는 이름인가
fn valid(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 맞지 않는 글자를 `_`로 바꾼 앞부분(해시를 붙일 자리를 남긴다). 비면 `tool`이다.
fn stem(id: &str) -> String {
    let stem: String = id
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                char::from(b)
            } else {
                '_'
            }
        })
        .take(MAX_NAME - SUFFIX)
        .collect();
    if stem.is_empty() {
        "tool".to_owned()
    } else {
        stem
    }
}

/// ID의 SHA-256 앞 4바이트(소문자 16진 8자리). `round`가 0이 아니면 겹침을 피하려고 섞는다.
fn suffix(id: &str, round: u32) -> String {
    let mut hash = Sha256::new();
    hash.update(id.as_bytes());
    hash.update(round.to_be_bytes());
    hash.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl NameTable {
    /// 이 요청에서 모델에게 보이거나 지난 차례에 나온 모든 도구 ID로 표를 만든다.
    pub(crate) fn new<'a>(ids: impl IntoIterator<Item = &'a ToolId>) -> Self {
        let unique: BTreeSet<&str> = ids.into_iter().map(|id| id.0.as_str()).collect();
        let mut alias_of = BTreeMap::new();
        let mut taken = BTreeSet::new();
        for id in unique.iter().filter(|id| valid(id)) {
            alias_of.insert((*id).to_owned(), (*id).to_owned());
            taken.insert((*id).to_owned());
        }
        for id in unique.iter().filter(|id| !valid(id)) {
            let base = stem(id);
            let mut round = 0u32;
            let alias = loop {
                let candidate = format!("{base}_{}", suffix(id, round));
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                round += 1;
            };
            alias_of.insert((*id).to_owned(), alias);
        }
        let tool_of = alias_of
            .iter()
            .map(|(id, alias)| (alias.clone(), ToolId(id.clone())))
            .collect();
        Self { alias_of, tool_of }
    }

    /// 모델에게 보이는 별칭
    pub(crate) fn alias(&self, id: &ToolId) -> Option<&str> {
        self.alias_of.get(&id.0).map(String::as_str)
    }

    /// 응답의 이름을 도구 ID로 되돌린다. 표에 없는 이름은 `None`이다.
    pub(crate) fn tool(&self, alias: &str) -> Option<&ToolId> {
        self.tool_of.get(alias)
    }
}
