//! `bot` — 프로필 폴더를 연결하면 같은 봇으로 대화하고 작업을 예약한다.
//! 종료 코드: 0 성공 · 1 실패 · 2 사용·설정 오류 · 3 다시 시도할 수 있는 실패(`bot chat`).
use bot_cli::{
    chat,
    cli::{BundleAction, Cli, Command},
    map, profile_bundle, serve,
    settings::Settings,
    validate,
};
use clap::Parser;
use std::process::ExitCode;

fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("bot: {message}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    bot_cli::logging::init();
    let cli = Cli::parse();
    let settings = match Settings::from_process() {
        Ok(settings) => settings,
        Err(message) => {
            eprintln!("bot: {message}");
            return ExitCode::from(2);
        }
    };
    match cli.command {
        Command::Validate => report(validate::run(&settings).await),
        Command::Chat(args) => chat::run(&settings, args).await,
        Command::Map(args) => report(map::run(&settings, args.json, args.profile.as_deref())),
        Command::Serve(args) => report(serve::run(&settings, &args.role, args.activate).await),
        Command::ProfileBundle(args) => match args.action {
            BundleAction::Put { dir } => report(profile_bundle::put(&settings, &dir).await),
        },
    }
}
