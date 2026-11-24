//! `bot map` — 검증한 프로필의 구조 지도. 이름·분류·개수만 싣는다(지침·지식 본문·자격 값은 읽지 않는다).
//! 카드와 연결은 관리 화면(`bot-admin`)과 같은 조립을 쓴다.
use crate::{load::load, settings::Settings, validate::known_profile};
use bot_admin::map::{Card, Kind, Map, build, to_json};
use bot_core::id::ProfileId;
use std::collections::BTreeMap;

const ORDER: [Kind; 8] = [
    Kind::Profile,
    Kind::Agent,
    Kind::Skill,
    Kind::Tool,
    Kind::Trigger,
    Kind::Binding,
    Kind::Scope,
    Kind::Secret,
];

pub fn markdown(map: &Map) -> String {
    let mut by_profile: BTreeMap<&str, Vec<&Card>> = BTreeMap::new();
    for card in &map.nodes {
        by_profile.entry(&card.profile).or_default().push(card);
    }
    let mut out = format!("# 구조 지도 (판본 {})\n", map.rev);
    for (profile, cards) in by_profile {
        out.push_str(&format!("\n## 프로필 {profile}\n"));
        for kind in ORDER {
            let of_kind: Vec<&&Card> = cards.iter().filter(|c| c.kind == kind).collect();
            if of_kind.is_empty() {
                continue;
            }
            out.push_str(&format!("\n### {}\n", kind.as_str()));
            for card in of_kind {
                out.push_str(&format!("- **{}** — {}\n", card.name, card.summary));
                if !card.facts.is_empty() {
                    let facts: Vec<String> =
                        card.facts.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    out.push_str(&format!("  - {}\n", facts.join(", ")));
                }
                for (rel, to) in &card.links {
                    out.push_str(&format!("  - {} → {to}\n", rel.as_str()));
                }
            }
        }
    }
    out
}

pub fn run(settings: &Settings, json: bool, profile: Option<&str>) -> Result<(), String> {
    let loaded = load(settings)?;
    let mut active = (**loaded.set.active()).clone();
    if let Some(name) = profile {
        if !known_profile(loaded.set.active(), name) {
            return Err(format!("프로필 {name}이(가) 없어요"));
        }
        active.retain(|id, _| *id == ProfileId(name.to_owned()));
    }
    let map = build(&active);
    if json {
        let text = serde_json::to_string_pretty(&to_json(&map, true)).map_err(|e| e.to_string())?;
        println!("{text}");
    } else {
        print!("{}", markdown(&map));
    }
    Ok(())
}
