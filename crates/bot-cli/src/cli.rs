//! CLI definition; each subcommand is implemented in the matching module.
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "bot",
    version,
    about = "Run bots and scheduled tasks from declarative profile directories",
    after_help = "Env: BOT_PROFILES (profile root) · BOT_HOST_CONFIG (host registry JSON; demo if unset) · BOT_DATABASE_URL · BOT_SOCKET · BOT_SECRETS"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Validate profiles and host config without connecting to DB or Slack
    Validate,
    /// Chat with an agent via the local CLI gateway
    Chat(ChatArgs),
    /// Print the structure map of loaded profiles
    Map(MapArgs),
    /// Start one or more runtime roles
    Serve(ServeArgs),
    /// Manage stored profile revision bundles for restoring historical revisions
    ProfileBundle(BundleArgs),
    /// Create a new workspace (profile directory + sample host.json) without overwriting existing files
    Init(InitArgs),
    /// Scaffold an agent, skill, or tool entry without modifying profile.yaml permissions
    Add(AddArgs),
    /// Check host, profile, secret, and DB configuration (offline by default; exits non-zero on failure)
    Doctor(DoctorArgs),
    /// Lock MCP server tool definitions into a skill directory
    Mcp(McpArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Template {
    /// Single-agent personal assistant
    Personal,
    /// Team lead with developer, designer, and planner sub-agents
    Team,
    /// Manager with product and support teams
    Company,
}

#[derive(Args)]
pub struct InitArgs {
    /// Workspace directory name and profile ID (lowercase letters, digits, hyphens)
    pub name: String,
    /// Starter template
    #[arg(long, value_enum, default_value = "personal")]
    pub template: Template,
    /// Existing parent directory in which to create `<name>/` (default: current directory)
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

#[derive(Args)]
pub struct AddArgs {
    #[command(subcommand)]
    pub what: AddKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ToolClass {
    /// Read-only tool
    Read,
    /// State-changing tool (requires human approval when listed in `policy.approve`)
    Write,
    /// Irreversible tool (always requires human approval)
    Irreversible,
}

#[derive(Subcommand)]
pub enum AddKind {
    /// Add an agent skeleton (AGENT.md) by lineage path (e.g. main/cs)
    Agent {
        /// Agent lineage path (parent agent must already exist)
        path: String,
        /// Profile directory containing profile.yaml (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Add a skill skeleton (SKILL.md)
    Skill {
        /// Skill name (lowercase letters, digits, hyphens)
        name: String,
        /// Attach skill to this agent (omit for profile-wide skill)
        #[arg(long)]
        agent: Option<String>,
        /// Profile directory containing profile.yaml (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Add a tool entry to a skill's tools.yaml without changing permission caps
    Tool {
        /// Tool ID (`<skill>.<tool>`, lowercase letters, digits, hyphens, e.g. agenda.search)
        id: String,
        /// Risk class
        #[arg(long, value_enum, default_value = "read")]
        class: ToolClass,
        /// Tool server alias (defaults to existing tools.yaml server or skill name)
        #[arg(long)]
        server: Option<String>,
        /// Agent lineage owning the skill (omit to search the profile)
        #[arg(long)]
        agent: Option<String>,
        /// Profile directory containing profile.yaml (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

#[derive(Args)]
pub struct DoctorArgs {
    /// Path to host.json (defaults to BOT_HOST_CONFIG, or built-in demo host if unset)
    #[arg(long)]
    pub host: Option<PathBuf>,
    /// Profiles root directory (defaults to BOT_PROFILES)
    #[arg(long)]
    pub profiles: Option<PathBuf>,
    /// Test a brief read-only database connection (no writes or migrations)
    #[arg(long)]
    pub check_db: bool,
}

#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub action: McpAction,
}

#[derive(Subcommand)]
pub enum McpAction {
    /// Fetch tool definitions declared in tools.yaml from a remote HTTP MCP server into tools.lock.yaml
    Sync {
        /// Skill directory under BOT_PROFILES (e.g. `<profile>/skills/<name>`)
        dir: std::path::PathBuf,
    },
}

#[derive(Args)]
pub struct BundleArgs {
    #[command(subcommand)]
    pub action: BundleAction,
}

#[derive(Subcommand)]
pub enum BundleAction {
    /// Backfill a historical profile directory bundle into the store for in-flight runs
    Put {
        /// Profile directory (directory name must match profile.yaml id)
        dir: std::path::PathBuf,
    },
}

#[derive(Args)]
pub struct ChatArgs {
    /// Profile to target when multiple profiles have CLI bindings
    #[arg(long)]
    pub profile: Option<String>,
    /// Thread ID to continue (omit to start a new thread per request)
    #[arg(long)]
    pub thread: Option<String>,
    /// Idempotent request ID reused across retries (generated if omitted)
    #[arg(long)]
    pub request_id: Option<String>,
    /// Return immediately once the request is durably accepted
    #[arg(long)]
    pub no_wait: bool,
    /// Wait for the result of an existing --request-id without accepting a new request
    #[arg(long, requires = "request_id", conflicts_with = "text")]
    pub attach: bool,
    /// Message text to send
    pub text: Option<String>,
}

#[derive(Args)]
pub struct MapArgs {
    /// Output JSON instead of Markdown
    #[arg(long)]
    pub json: bool,
    /// Limit output to a single profile
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Roles to start (comma-separated); `all` starts every role
    #[arg(long, value_enum, value_delimiter = ',', default_value = "all")]
    pub role: Vec<Role>,
    /// Explicitly activate this revision when it differs from the published active revision
    #[arg(long)]
    pub activate: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, ValueEnum)]
pub enum Role {
    All,
    Gateway,
    Scheduler,
    Worker,
    // Hidden from help; selecting it parses so serve can reject with a clear explanation that broker runs inside worker.
    #[value(hide = true)]
    Broker,
    Admin,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::All => "all",
            Role::Gateway => "gateway",
            Role::Scheduler => "scheduler",
            Role::Worker => "worker",
            Role::Broker => "broker",
            Role::Admin => "admin",
        }
    }

    pub fn expand(roles: &[Role]) -> Vec<Role> {
        let mut out: Vec<Role> = roles
            .iter()
            .flat_map(|role| match role {
                // Broker runs embedded in worker, not as a standalone role.
                Role::All => vec![Role::Gateway, Role::Scheduler, Role::Worker, Role::Admin],
                other => vec![*other],
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn all_expands_to_every_role_once() {
        let all = Role::expand(&[Role::All, Role::Gateway]);
        assert_eq!(
            all,
            [Role::Gateway, Role::Scheduler, Role::Worker, Role::Admin],
            "all omits broker because it runs inside worker"
        );
        assert_eq!(Role::expand(&[Role::Worker, Role::Worker]), [Role::Worker]);
    }

    #[test]
    fn parses_the_documented_commands() {
        let chat = Cli::try_parse_from([
            "bot",
            "chat",
            "--profile",
            "personal",
            "Show today's agenda",
        ])
        .unwrap();
        match chat.command {
            Command::Chat(args) => {
                assert_eq!(args.profile.as_deref(), Some("personal"));
                assert_eq!(args.text.as_deref(), Some("Show today's agenda"));
            }
            _ => panic!(),
        }
        let serve = Cli::try_parse_from(["bot", "serve", "--role", "gateway,worker"]).unwrap();
        match serve.command {
            Command::Serve(args) => assert_eq!(args.role, [Role::Gateway, Role::Worker]),
            _ => panic!(),
        }
        match Cli::try_parse_from(["bot", "serve"]).unwrap().command {
            Command::Serve(args) => {
                assert_eq!((args.role, args.activate), (vec![Role::All], false))
            }
            _ => panic!(),
        }
        match Cli::try_parse_from(["bot", "serve", "--activate"])
            .unwrap()
            .command
        {
            Command::Serve(args) => assert!(args.activate),
            _ => panic!(),
        }
        assert!(Cli::try_parse_from(["bot", "chat", "--attach"]).is_err());
        assert!(
            Cli::try_parse_from(["bot", "chat", "--attach", "--request-id", "r1", "text"]).is_err()
        );
        assert!(Cli::try_parse_from(["bot", "serve", "--role", "nope"]).is_err());
        match Cli::try_parse_from(["bot", "init", "my-bot", "--template", "team"])
            .unwrap()
            .command
        {
            Command::Init(args) => {
                assert_eq!(
                    (args.name.as_str(), args.template),
                    ("my-bot", Template::Team)
                );
                assert_eq!(args.dir, PathBuf::from("."));
            }
            _ => panic!(),
        }
        match Cli::try_parse_from(["bot", "add", "skill", "triage", "--agent", "main/cs"])
            .unwrap()
            .command
        {
            Command::Add(AddArgs {
                what: AddKind::Skill { name, agent, .. },
            }) => assert_eq!(
                (name.as_str(), agent.as_deref()),
                ("triage", Some("main/cs"))
            ),
            _ => panic!(),
        }
        match Cli::try_parse_from([
            "bot",
            "add",
            "tool",
            "weather.forecast",
            "--class",
            "write",
            "--server",
            "weather-mcp",
            "--agent",
            "main",
        ])
        .unwrap()
        .command
        {
            Command::Add(AddArgs {
                what:
                    AddKind::Tool {
                        id,
                        class,
                        server,
                        agent,
                        ..
                    },
            }) => assert_eq!(
                (id.as_str(), class, server.as_deref(), agent.as_deref()),
                (
                    "weather.forecast",
                    ToolClass::Write,
                    Some("weather-mcp"),
                    Some("main")
                )
            ),
            _ => panic!(),
        }
        assert!(Cli::try_parse_from(["bot", "add", "tool"]).is_err());
        assert!(Cli::try_parse_from(["bot", "add", "tool", "a.b", "--class", "bad"]).is_err());
        assert!(Cli::try_parse_from(["bot", "add", "agent"]).is_err());
        match Cli::try_parse_from(["bot", "doctor", "--check-db", "--host", "h.json"])
            .unwrap()
            .command
        {
            Command::Doctor(args) => {
                assert!(args.check_db);
                assert_eq!(args.host, Some(PathBuf::from("h.json")));
            }
            _ => panic!(),
        }
        match Cli::try_parse_from(["bot", "mcp", "sync", "profiles/personal/skills/weather"])
            .unwrap()
            .command
        {
            Command::Mcp(args) => match args.action {
                McpAction::Sync { dir } => {
                    assert_eq!(
                        dir,
                        std::path::PathBuf::from("profiles/personal/skills/weather")
                    );
                }
            },
            _ => panic!(),
        }
        assert!(Cli::try_parse_from(["bot", "mcp", "sync"]).is_err());
        let hidden = Cli::command()
            .find_subcommand("serve")
            .unwrap()
            .get_arguments()
            .find(|arg| arg.get_id() == "role")
            .unwrap()
            .get_possible_values()
            .into_iter()
            .filter(|value| !value.is_hide_set())
            .map(|value| value.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            hidden,
            ["all", "gateway", "scheduler", "worker", "admin"],
            "broker must stay hidden from help"
        );
        match Cli::try_parse_from(["bot", "serve", "--role", "broker"])
            .unwrap()
            .command
        {
            Command::Serve(args) => assert_eq!(args.role, [Role::Broker]),
            _ => panic!(),
        }
    }
}
