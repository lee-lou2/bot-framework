//! 명령줄 정의. 각 명령의 동작은 같은 이름의 모듈에 있다.
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "bot",
    version,
    about = "프로필 폴더를 연결하면 같은 봇으로 대화하고 작업을 예약합니다",
    after_help = "환경: BOT_PROFILES(프로필 폴더) · BOT_HOST_CONFIG(호스트 등록부 JSON, 없으면 데모) · BOT_DATABASE_URL · BOT_SOCKET · BOT_SECRETS"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// 프로필·호스트 설정을 검증한다(DB·Slack 접속 없이)
    Validate,
    /// 에이전트와 대화한다(이 사용자의 로컬 입구)
    Chat(ChatArgs),
    /// 프로필의 구조 지도를 출력한다
    Map(MapArgs),
    /// 역할을 시작한다
    Serve(ServeArgs),
    /// 프로필 판본의 묶음을 저장소에서 다룬다(재배포 뒤 옛 판본 복원용)
    ProfileBundle(BundleArgs),
    /// 새 작업 폴더를 만든다(프로필 폴더 + 호스트 설정 예시). 있는 것은 덮어쓰지 않는다
    Init(InitArgs),
    /// 프로필에 에이전트·스킬 뼈대를 더한다. 권한 상한(profile.yaml)은 바꾸지 않는다
    Add(AddArgs),
    /// 설정을 점검해 한국어로 요약한다(기본은 네트워크 접속 없이 설정만 본다). 실패 항목이 있으면 종료 코드가 0이 아니다
    Doctor(DoctorArgs),
    /// MCP 도구 서버의 도구 정의를 프로필에 고정한다
    Mcp(McpArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Template {
    /// 에이전트 하나: 가장 작은 개인 비서
    Personal,
    /// 팀 리드 아래에 개발·디자인·기획 에이전트
    Team,
    /// 매니저 아래에 제품·고객지원 팀과 팀원
    Company,
}

#[derive(Args)]
pub struct InitArgs {
    /// 만들 이름: 작업 폴더 이름이자 프로필 ID(소문자·숫자·하이픈)
    pub name: String,
    /// 시작 템플릿
    #[arg(long, value_enum, default_value = "personal")]
    pub template: Template,
    /// 이 폴더 안에 `<이름>/`을 만든다(이미 있는 폴더, 기본은 현재 폴더)
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

#[derive(Args)]
pub struct AddArgs {
    #[command(subcommand)]
    pub what: AddKind,
}

#[derive(Subcommand)]
pub enum AddKind {
    /// 에이전트 뼈대(AGENT.md)를 더한다. 경로는 계보로 쓴다(예: main/cs)
    Agent {
        /// 에이전트 경로(상위 에이전트는 이미 있어야 한다)
        path: String,
        /// 프로필 폴더(profile.yaml이 있는 곳, 기본은 현재 폴더)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// 스킬 뼈대(SKILL.md)를 더한다
    Skill {
        /// 스킬 이름(소문자·숫자·하이픈)
        name: String,
        /// 이 에이전트의 스킬로 둔다(생략하면 프로필 전체가 보는 스킬)
        #[arg(long)]
        agent: Option<String>,
        /// 프로필 폴더(profile.yaml이 있는 곳, 기본은 현재 폴더)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

#[derive(Args)]
pub struct DoctorArgs {
    /// 호스트 등록부(host.json) 경로. 생략하면 BOT_HOST_CONFIG, 그것도 없으면 내장 데모 호스트
    #[arg(long)]
    pub host: Option<PathBuf>,
    /// 프로필 루트 폴더(프로필 폴더들의 부모). 생략하면 BOT_PROFILES
    #[arg(long)]
    pub profiles: Option<PathBuf>,
    /// 데이터베이스에 짧게 접속해 연결만 확인한다(쓰기·마이그레이션 없음). 기본은 접속하지 않는다
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
    /// 스킬 폴더의 tools.yaml이 선언한 도구의 정의(이름·설명·입력 스키마)를 서버에서 받아 tools.lock.yaml에 적는다(원격 `transport: "http"` 서버)
    Sync {
        /// 스킬 폴더(BOT_PROFILES 아래 프로필의 `skills/<이름>`)
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
    /// 옛 판본의 원본 프로필 폴더를 저장소에 보관한다(이전 버전이 보관하지 않은 판본을 진행 중 작업이 아직 쓸 때 `bot serve`가 알려 준다)
    Put {
        /// 프로필 폴더(폴더 이름 = profile.yaml의 id)
        dir: std::path::PathBuf,
    },
}

#[derive(Args)]
pub struct ChatArgs {
    /// 여러 프로필에 입구가 있을 때 고른다
    #[arg(long)]
    pub profile: Option<String>,
    /// 같은 대화 스레드를 이어 간다(생략하면 요청마다 새 스레드)
    #[arg(long)]
    pub thread: Option<String>,
    /// 재시도에도 같아야 하는 요청 ID(생략하면 새로 만든다)
    #[arg(long)]
    pub request_id: Option<String>,
    /// 접수까지만 확인하고 돌아온다
    #[arg(long)]
    pub no_wait: bool,
    /// 새 접수 없이 --request-id의 결과를 다시 받는다
    #[arg(long, requires = "request_id", conflicts_with = "text")]
    pub attach: bool,
    /// 보낼 글
    pub text: Option<String>,
}

#[derive(Args)]
pub struct MapArgs {
    /// JSON으로 출력한다(기본은 Markdown)
    #[arg(long)]
    pub json: bool,
    /// 이 프로필만
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Args)]
pub struct ServeArgs {
    /// 시작할 역할(쉼표로 여러 개). all은 전 역할
    #[arg(long, value_enum, value_delimiter = ',', default_value = "all")]
    pub role: Vec<Role>,
    /// 저장소에 게시된 활성 판본이 이 프로필 폴더와 다를 때 이 폴더의 판본으로 교체한다(명시 활성화).
    /// 없으면 다른 판본이 게시돼 있는 한 시작하지 않는다(구판 역할이 새 판본을 덮지 않게).
    #[arg(long)]
    pub activate: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, ValueEnum)]
pub enum Role {
    All,
    Gateway,
    Scheduler,
    Worker,
    /// 브로커(도구 실행·승인)는 worker 안에서 돈다. 목록에서는 숨기고 요청하면 «worker에 포함»이라고 안내하며 거절한다.
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

    /// `all`을 풀어 중복 없이 정렬한 역할 목록
    pub fn expand(roles: &[Role]) -> Vec<Role> {
        let mut out: Vec<Role> = roles
            .iter()
            .flat_map(|role| match role {
                // 브로커는 worker에 포함돼 있어 별도 역할이 아니다
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
            "all에 브로커는 없다(worker에 포함)"
        );
        assert_eq!(Role::expand(&[Role::Worker, Role::Worker]), [Role::Worker]);
    }

    #[test]
    fn parses_the_documented_commands() {
        let chat =
            Cli::try_parse_from(["bot", "chat", "--profile", "personal", "오늘 일정 알려줘"])
                .unwrap();
        match chat.command {
            Command::Chat(args) => {
                assert_eq!(args.profile.as_deref(), Some("personal"));
                assert_eq!(args.text.as_deref(), Some("오늘 일정 알려줘"));
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
        // 재부착은 요청 ID가 있어야 하고 글과 함께 쓸 수 없다
        assert!(Cli::try_parse_from(["bot", "chat", "--attach"]).is_err());
        assert!(
            Cli::try_parse_from(["bot", "chat", "--attach", "--request-id", "r1", "글"]).is_err()
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
        // 도구 정의 고정: 스킬 폴더 하나를 받는다
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
        // 브로커는 목록에서 숨기지만 요청하면 파싱돼 이유와 함께 거절된다
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
            "도움말의 역할 목록에 broker는 없다"
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
