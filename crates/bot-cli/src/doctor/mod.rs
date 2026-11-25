//! `bot doctor` — 설정을 점검해 한국어로 요약한다. 기본은 설정만 본다: 네트워크 접속·마이그레이션·모델·도구 서버 시작이 없고,
//! 데이터베이스 연결은 `--check-db`를 줬을 때만 짧게 접속해 본다(쓰기 없음). 비밀은 **이름**만 나오고 값·접속 주소는 어디에도 출력하지
//! 않는다. «미검사»는 «통과»가 아니다. 실패 항목이 하나라도 있으면 오류로 끝난다(종료 코드 1).
//! 새 모델 제공자·도구 서버 종류는 `host.rs`의 `match`에 항목을 더해 점검한다(빠뜨리면 컴파일이 막는다).
mod database;
mod host;
mod profiles;

use crate::{cli::DoctorArgs, settings::Settings};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Pass,
    /// 고치지 않아도 시작은 되지만 알려 둘 것(종료 코드에 영향 없음)
    Warn,
    Fail,
    /// 확인하지 않았다(성공이 아니다)
    Skipped,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Pass => "통과",
            State::Warn => "주의",
            State::Fail => "실패",
            State::Skipped => "미검사",
        }
    }
}

pub struct Item {
    pub state: State,
    pub title: &'static str,
    pub summary: String,
    pub details: Vec<String>,
}

impl Item {
    pub fn new(state: State, title: &'static str, summary: impl Into<String>) -> Self {
        Self {
            state,
            title,
            summary: summary.into(),
            details: Vec::new(),
        }
    }

    pub fn with(mut self, details: impl IntoIterator<Item = String>) -> Self {
        self.details.extend(details);
        self
    }

    /// 앞 단계가 실패해 확인하지 못한 항목
    pub fn skipped(title: &'static str, why: &str) -> Self {
        Self::new(State::Skipped, title, why)
    }
}

/// 긴 목록은 앞부분만 보여 준다(출력이 점검 결과를 가리지 않게).
pub fn capped(lines: impl IntoIterator<Item = String>, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = lines.into_iter().collect();
    if lines.len() > max {
        let rest = lines.len() - max;
        lines.truncate(max);
        lines.push(format!("… 외 {rest}줄"));
    }
    lines
}

fn render(items: &[Item]) -> String {
    let mut lines =
        vec!["bot doctor — 설정 점검(기본은 네트워크 접속 없이 설정만 봐요)".to_owned()];
    for item in items {
        lines.push(format!(
            "[{}] {} — {}",
            item.state.label(),
            item.title,
            item.summary
        ));
        lines.extend(item.details.iter().map(|line| format!("    {line}")));
    }
    let count = |state| items.iter().filter(|item| item.state == state).count();
    lines.push(format!(
        "요약: 통과 {} · 주의 {} · 실패 {} · 미검사 {}",
        count(State::Pass),
        count(State::Warn),
        count(State::Fail),
        count(State::Skipped)
    ));
    lines.join("\n")
}

pub async fn run(settings: &Settings, args: &DoctorArgs) -> Result<(), String> {
    let mut settings = settings.clone();
    if let Some(path) = &args.host {
        settings.host_config = Some(path.clone());
    }
    if let Some(path) = &args.profiles {
        settings.profiles = Some(path.clone());
    }
    let mut items = Vec::new();
    let host = host::check(&settings, &mut items);
    profiles::check(&settings, host.as_ref(), &mut items).await;
    database::check(&settings, args.check_db, &mut items).await;
    println!("{}", render(&items));
    match items
        .iter()
        .filter(|item| item.state == State::Fail)
        .count()
    {
        0 => Ok(()),
        failed => Err(format!(
            "점검에서 실패한 항목이 {failed}개예요 — 위 [실패] 항목의 안내대로 고쳐 주세요"
        )),
    }
}
