//! `bot` binary entry point (exit codes: 0 ok, 1 error, 2 config/usage, 3 retryable chat error).
use bot_cli::{
    auth, chat,
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
    // Scaffolding commands bypass env settings so invalid `BOT_*` vars do not block bootstrapping.
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
        Command::Init(_) | Command::Add(_) => unreachable!("handled above"),
        Command::Mcp(args) => match args.action {
            McpAction::Sync { dir } => report(mcp_sync::run(&settings, &dir).await),
        },
        Command::Auth(args) => report(auth::cli::run(&settings, args).await),
    }
}

fn init(args: &bot_cli::cli::InitArgs) -> Result<(), String> {
    let uid = bot_gateway::cli::socket::current_uid()
        .map_err(|_| "failed to determine OS user ID (id -u)".to_owned())?;
    let text = scaffold::init(&args.name, args.template, &args.dir, uid)?;
    println!("{text}");
    Ok(())
}

fn add(args: &AddArgs) -> Result<(), String> {
    let text = match &args.what {
        AddKind::Agent { path, dir } => scaffold::agent(dir, path)?,
        AddKind::Skill { name, agent, dir } => scaffold::skill(dir, name, agent.as_deref())?,
        AddKind::Tool {
            id,
            class,
            server,
            agent,
            dir,
        } => scaffold::tool(dir, id, *class, server.as_deref(), agent.as_deref())?,
    };
    println!("{text}");
    Ok(())
}
