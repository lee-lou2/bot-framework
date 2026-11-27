//! Knowledge selection tests: word/bigram overlap, source attribution, and non-truncating limits.

use super::*;

#[test]
fn knowledge_is_chosen_by_word_overlap_with_sources_and_limits() {
    let mut snap = snapshot();
    let knowledge = vec![
        doc("knowledge/travel.md", "Trip plans & flight booking"),
        doc(
            "knowledge/agenda.md",
            "Agenda rules: check every agenda item.",
        ),
        doc("knowledge/cooking.md", "Muffin mix for soup"),
        doc(
            "agents/main/knowledge/calendar-tips.md",
            "calendar agenda tips",
        ),
    ];
    snap.agents
        .get_mut(&AgentId("main".into()))
        .unwrap()
        .knowledge = knowledge.clone();
    let run = run_context("main", &FULL, "My agenda check");
    let out = build(&snap, &run, &[], &[]);
    assert_eq!(
        out.knowledge,
        [
            "knowledge/agenda.md",
            "agents/main/knowledge/calendar-tips.md"
        ]
    );
    assert!(out.system.contains("## Reference Documents\n### knowledge/agenda.md\nAgenda rules: check every agenda item.\n\n### agents/main/knowledge/calendar-tips.md\ncalendar agenda tips"));
    // Inflected/plural forms ("agendas" <-> "agenda") still match through character bigrams.
    assert!(
        select_knowledge("agendas", &knowledge)
            .iter()
            .any(|d| d.relative_path == "knowledge/agenda.md")
    );
    assert!(
        select_knowledge("", &knowledge).is_empty()
            && select_knowledge("a !", &knowledge).is_empty()
    );
}

#[test]
fn knowledge_respects_the_document_and_byte_limits_without_cutting_documents() {
    let many: Vec<Document> = (0..12)
        .map(|n| doc(&format!("k/d{n:02}.md"), "shared word agenda"))
        .collect();
    let picked = select_knowledge("shared agenda", &many);
    assert_eq!(picked.len(), MAX_KNOWLEDGE_DOCS);
    assert_eq!(picked[0].relative_path, "k/d00.md", "ties break by path");

    let big = |name: &str, filler: usize| doc(name, &format!("agenda {}", "€".repeat(filler)));
    let docs = vec![
        big("a-high.md", 6000),
        big("b.md", 6000),
        doc("c-small.md", "agenda agenda agenda"),
    ];
    let picked = select_knowledge("agenda", &docs);
    let total: usize = picked.iter().map(|d| d.text.len()).sum();
    assert!(total <= MAX_KNOWLEDGE_BYTES, "{total}");
    assert!(picked.iter().all(|d| {
        d.text.len()
            == docs
                .iter()
                .find(|x| x.relative_path == d.relative_path)
                .unwrap()
                .text
                .len()
    }));
    assert_eq!(
        picked.len(),
        2,
        "only one 18 KiB document fits, followed by the small document"
    );
    let names: Vec<_> = picked.iter().map(|d| d.relative_path.as_str()).collect();
    assert!(names.contains(&"c-small.md"), "{names:?}");

    let huge = doc("huge.md", &format!("agenda {}", "€".repeat(11_000)));
    let small = doc("small.md", "agenda note");
    let both = [huge, small];
    let picked = select_knowledge("agenda", &both);
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].relative_path, "small.md");
}
