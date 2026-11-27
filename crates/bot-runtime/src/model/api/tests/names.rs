//! Tool name alias table tests: upstream syntax, uniqueness, determinism, and reversibility.

use super::super::names::NameTable;
use bot_core::id::ToolId;

fn ids(items: &[&str]) -> Vec<ToolId> {
    items.iter().map(|item| ToolId((*item).into())).collect()
}

fn rule(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[test]
fn valid_ids_keep_their_names_and_dotted_ones_get_a_stable_alias_that_cannot_collide() {
    let all = ids(&["agenda.list", "agenda_list", "delegate", "agenda.delete"]);
    let table = NameTable::new(&all);
    assert_eq!(table.alias(&all[2]), Some("delegate"));
    assert_eq!(table.alias(&all[1]), Some("agenda_list"));
    let dotted = table.alias(&all[0]).unwrap();
    assert_ne!(dotted, "agenda_list");
    assert!(dotted.starts_with("agenda_list_") && dotted.len() == "agenda_list_".len() + 8);
    let mut seen = std::collections::BTreeSet::new();
    for id in &all {
        let alias = table.alias(id).unwrap();
        assert!(rule(alias), "{alias}");
        assert!(seen.insert(alias.to_owned()), "{alias}");
        assert_eq!(table.tool(alias), Some(id));
    }
    for fake in ["agenda.list", "Delegate", "agenda_list ", "unknown", ""] {
        assert!(table.tool(fake).is_none(), "{fake:?}");
    }
}

#[test]
fn the_table_does_not_depend_on_the_input_order_or_repeats() {
    let a = ids(&["x.a", "x.b", "y.c", "x.a"]);
    let b = ids(&["y.c", "x.b", "x.a"]);
    let (ta, tb) = (NameTable::new(&a), NameTable::new(&b));
    for id in &b {
        assert_eq!(ta.alias(id), tb.alias(id));
    }
}

#[test]
fn long_non_ascii_and_odd_ids_still_become_valid_unique_aliases() {
    let long = "€".repeat(40);
    let longer = format!("skill.{}", "t".repeat(100));
    let all = ids(&[
        long.as_str(),
        "★★.αβ",
        "★★.γδ",
        longer.as_str(),
        "a b",
        ".",
        "",
        "x".repeat(64).as_str(),
        "x".repeat(65).as_str(),
    ]);
    let table = NameTable::new(&all);
    let mut seen = std::collections::BTreeSet::new();
    for id in &all {
        let alias = table.alias(id).unwrap();
        assert!(rule(alias), "{id:?} -> {alias}");
        assert!(seen.insert(alias.to_owned()), "{alias}");
        assert_eq!(table.tool(alias), Some(id));
    }
    assert_eq!(table.alias(&all[7]), Some("x".repeat(64).as_str()));
    assert_ne!(table.alias(&all[8]), Some("x".repeat(65).as_str()));
}
