//! Bijective per-request tool ID <-> upstream alias table enforcing `^[a-zA-Z0-9_-]{1,64}$`.
//!
//! Valid IDs keep their exact name; invalid ones map to `<sanitized-stem>_<sha256-8hex>` in sorted
//! order (with collision rounds) so `a.b` and `a_b` never collide and fabricated names return `None`.

use bot_core::id::ToolId;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_NAME: usize = 64;
const SUFFIX: usize = 9; // '_' + 8 hex chars

#[derive(Debug)]
pub(crate) struct NameTable {
    alias_of: BTreeMap<String, String>,
    tool_of: BTreeMap<String, ToolId>,
}

fn valid(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

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

    pub(crate) fn alias(&self, id: &ToolId) -> Option<&str> {
        self.alias_of.get(&id.0).map(String::as_str)
    }

    pub(crate) fn tool(&self, alias: &str) -> Option<&ToolId> {
        self.tool_of.get(alias)
    }
}
