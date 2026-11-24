//! 지식 고르기 시험: 낱말·글자쌍 겹침 · 출처 · 문서 수와 바이트 한도 · 문서를 자르지 않기.

use super::*;

#[test]
fn knowledge_is_chosen_by_word_overlap_with_sources_and_limits() {
    let mut snap = snapshot();
    let knowledge = vec![
        doc("knowledge/travel.md", "여행 계획과 항공권 예약 방법"),
        doc(
            "knowledge/agenda.md",
            "일정을 관리하는 규칙: 일정은 하루 단위로 확인한다.",
        ),
        doc("knowledge/cooking.md", "요리 레시피와 장보기 목록"),
        doc(
            "agents/main/knowledge/calendar-tips.md",
            "calendar 일정 정리 팁",
        ),
    ];
    snap.agents
        .get_mut(&AgentId("main".into()))
        .unwrap()
        .knowledge = knowledge.clone();
    let run = run_context("main", &FULL, "오늘 일정을 알려줘");
    let out = build(&snap, &run, &[], &[]);
    // 관련 없는 문서(travel·cooking)는 빠지고, 겹침이 큰 문서가 앞선다
    assert_eq!(
        out.knowledge,
        [
            "knowledge/agenda.md",
            "agents/main/knowledge/calendar-tips.md"
        ]
    );
    assert!(out.system.contains("## 참고 문서\n### knowledge/agenda.md\n일정을 관리하는 규칙: 일정은 하루 단위로 확인한다.\n\n### agents/main/knowledge/calendar-tips.md\ncalendar 일정 정리 팁"));
    // 한글 조사("일정을" ↔ "일정")가 붙어도 글자쌍으로 겹침을 잡는다
    assert!(
        select_knowledge("일정을", &knowledge)
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
    // 12개가 모두 겹친다: 문서 수 한도 8
    let many: Vec<Document> = (0..12)
        .map(|n| doc(&format!("k/d{n:02}.md"), "공통 단어 일정"))
        .collect();
    let picked = select_knowledge("공통 일정", &many);
    assert_eq!(picked.len(), MAX_KNOWLEDGE_DOCS);
    assert_eq!(picked[0].relative_path, "k/d00.md", "점수가 같으면 경로 순");

    // 바이트 한도: 큰 문서(20 KiB) 둘은 함께 못 들어간다. 점수가 높은 쪽이 남고 문서 중간을 자르지 않는다.
    let big = |name: &str, filler: usize| doc(name, &format!("일정 {}", "가".repeat(filler)));
    let docs = vec![
        big("a-high.md", 6000),
        big("b.md", 6000),
        doc("c-small.md", "일정 일정 일정"),
    ];
    let picked = select_knowledge("일정", &docs);
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
        "6천 자(18 KiB)짜리는 하나만 들어가고 작은 문서가 이어서 들어간다"
    );
    let names: Vec<_> = picked.iter().map(|d| d.relative_path.as_str()).collect();
    assert!(names.contains(&"c-small.md"), "{names:?}");

    // 한 문서가 한도보다 크면 그 문서만 빠지고 다음 문서를 본다
    let huge = doc("huge.md", &format!("일정 {}", "가".repeat(11_000)));
    let small = doc("small.md", "일정 메모");
    let both = [huge, small];
    let picked = select_knowledge("일정", &both);
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].relative_path, "small.md");
}
