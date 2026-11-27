//! `bot doctor` offline configuration check and optional read-only DB connectivity probe.
// Never prints secret values or DB URLs; `Skipped` is distinct from `Pass`, and any `Fail` exits non-zero.
mod database;
mod host;
mod profiles;

use crate::{cli::DoctorArgs, settings::Settings};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Pass,
    /// Advisory notice; does not fail the exit code.
    Warn,
    Fail,
    /// Not checked (not equivalent to `Pass`).
    Skipped,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Pass => "PASS",
            State::Warn => "WARN",
            State::Fail => "FAIL",
            State::Skipped => "SKIP",
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

    pub fn skipped(title: &'static str, why: &str) -> Self {
        Self::new(State::Skipped, title, why)
    }
}

pub fn capped(lines: impl IntoIterator<Item = String>, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = lines.into_iter().collect();
    if lines.len() > max {
        let rest = lines.len() - max;
        lines.truncate(max);
        lines.push(format!("... and {rest} more"));
    }
    lines
}

fn render(items: &[Item]) -> String {
    let mut lines = vec!["bot doctor — configuration check (offline by default)".to_owned()];
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
        "Summary: {} pass · {} warn · {} fail · {} skipped",
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
            "{failed} check(s) failed — fix the [FAIL] items above"
        )),
    }
}
