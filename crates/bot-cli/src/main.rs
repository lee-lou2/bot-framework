//! `bot` — 프로필 폴더를 연결하면 같은 봇으로 대화하고 작업을 예약한다.
//! 종료 코드: 0 성공 · 1 실패 · 2 사용·설정 오류 · 3 다시 시도할 수 있는 실패(`bot chat`).
use bot_cli::{
    chat,
    cli::{AddArgs, AddKind, BundleAction, Cli, Command, McpAction},
    doctor, map, mcp_sync, profile_bundle, scaffold, serve,
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
    // 새 파일을 만드는 명령은 환경 설정을 읽지 않는다(잘못된 BOT_ 변수가 있어도 시작할 수 있다)
    match &cli.command {
        Command::Init(args) => return report(init(args)),
        Command::Add(args) => return report(add(args)),
        _ => {}
    }
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
        Command::Doctor(args) => report(doctor::run(&settings, &args).await),
        Command::Init(_) | Command::Add(_) => unreachable!("위에서 처리했다"),
        Command::Mcp(args) => match args.action {
            McpAction::Sync { dir } => report(mcp_sync::run(&settings, &dir).await),
        },
    }
}

fn init(args: &bot_cli::cli::InitArgs) -> Result<(), String> {
    let uid = bot_gateway::cli::socket::current_uid()
        .map_err(|_| "OS 사용자 번호(id -u)를 확인하지 못했어요".to_owned())?;
    let text = scaffold::init(&args.name, args.template, &args.dir, uid)?;
    println!("{text}");
    Ok(())
}

fn add(args: &AddArgs) -> Result<(), String> {
    let text = match &args.what {
        AddKind::Agent { path, dir } => scaffold::agent(dir, path)?,
        AddKind::Skill { name, agent, dir } => scaffold::skill(dir, name, agent.as_deref())?,
    };
    println!("{text}");
    Ok(())
}
