use super::*;
use bot_core::{
    command::JsonPayload,
    id::{AgentId, ToolId},
};

#[tokio::test]
async fn both_dialects_send_tool_descriptions_from_the_snapshot() {
    let Some(w) = world().await else { return };
    let mut source = (*profile()).clone();
    let main = source.agents.get_mut(&AgentId("main".into())).unwrap();
    let tools = &mut main.skills.get_mut("agenda").unwrap().tools;
    let list = tools.get_mut(&ToolId("agenda.list".into())).unwrap();
    let locked_description = "일정 제목을 \"그대로\" 조회합니다.\n날짜를 입력하세요.";
    list.description = Some(locked_description.into());
    list.input = JsonPayload(r#"{"type":"object","description":"스키마 설명"}"#.into());
    let moved = tools.get_mut(&ToolId("agenda.move".into())).unwrap();
    moved.input = JsonPayload(r#"{"type":"object","description":"일정을 옮깁니다."}"#.into());
    let deleted = tools.get_mut(&ToolId("agenda.delete".into())).unwrap();
    deleted.input = JsonPayload(r#"{"type":"object","description":""}"#.into());
    w.catalog.publish(Arc::new(source));

    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "확인했어요", None));
        w.accept(&format!("descriptions-{kind:?}"), "일정을 확인해 주세요")
            .await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        let seen = api.seen();
        let request = seen[0].json();
        let declared = request["tools"].as_array().unwrap();
        for (id, description) in [
            ("agenda.list", locked_description),
            ("agenda.move", "일정을 옮깁니다."),
            ("agenda.delete", "agenda.delete"),
        ] {
            let alias = seen[0].alias(id);
            let actual = declared
                .iter()
                .map(|tool| match kind {
                    ApiKind::Anthropic => tool,
                    ApiKind::OpenAi => &tool["function"],
                })
                .find(|tool| tool["name"] == alias)
                .unwrap();
            assert_eq!(actual["description"], description, "{kind:?} {id}");
        }
    }
}
