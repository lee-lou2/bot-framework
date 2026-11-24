//! CLI 실행기(Claude Code·Pi) 공통: 설정, 호출별 폴더, 결과 변환.
//! 호출마다 `<workspace>/<이름>/in`(읽기 전용으로 마운트: 지침·도구 설정)과 `.../work`(호출 전용 쓰기 공간)를 만들고 끝나면 지운다.
//! 폴더는 매번 **새로 배타적으로**(이미 있으면 만들지 않고 다른 이름으로 다시) 0700으로 만든다: 프로세스를 다시 띄웠거나 같은 run·세대에 claim만 다른
//! 두 프로세스가 서로의 폴더를 읽거나 지우지 못한다. 정리는 자기가 만든 폴더만 한다.

use super::{ModelError, ModelRequest, ModelStep, Output, RelayChannel};
use bot_core::id::Attempt;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// 입력 폴더가 붙는 곳이자 CLI의 작업 폴더
pub(crate) const BOT_MOUNT: &str = "/bot";
/// 호출 전용 쓰기 공간이 붙는 곳
pub(crate) const WORK_MOUNT: &str = "/work";

/// 모델 별칭 하나가 가리키는 실제 모델. 별칭 해석은 호스트 설정의 몫이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliModel {
    pub id: String,
    pub effort: String,
    pub max_output_tokens: u32,
    pub retries: u32,
    pub context_window: u64,
}

/// CLI 실행기 설정. 비밀은 없다: 모델 자격은 호스트가 브로커 프런트로 중계하고, 실행기는 run마다 받는 프런트 주소·세션 토큰(`RelayChannel`)만 쓴다.
#[derive(Clone, Debug)]
pub struct CliConfig {
    pub models: BTreeMap<String, CliModel>,
    /// 호출별 입력·임시 폴더를 만드는 호스트 폴더
    pub workspace: PathBuf,
    /// 실행 파일 이름 덮어쓰기(기본 `claude` / `pi`)
    pub program: Option<String>,
    /// Pi가 모델 설정에 쓰는 프로바이더 이름
    pub pi_provider: String,
    /// 샌드박스가 CLI의 파일 읽기를 마운트한 입력 폴더 안으로 가둔다고 호스트가 보증한다(VM 어댑터). 읽기 전용 내장 도구(`Agent.builtin`)는
    /// 이 보증이 있을 때만 켠다: 가둘 수 없으면 내장 도구를 요구하는 실행을 거절한다.
    pub confined_reads: bool,
}

impl CliConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            models: BTreeMap::new(),
            workspace: workspace.into(),
            program: None,
            pi_provider: "relay".into(),
            confined_reads: false,
        }
    }

    /// 읽기가 마운트 안으로 갇힌다고 보증한다(VM 샌드박스를 쓸 때).
    pub fn with_confined_reads(mut self) -> Self {
        self.confined_reads = true;
        self
    }

    pub fn with_model(mut self, alias: impl Into<String>, model: CliModel) -> Self {
        self.models.insert(alias.into(), model);
        self
    }
}

/// 호출 하나의 폴더. 떨어지면(성공·실패·취소 모두) 지운다.
pub(crate) struct CallDir {
    root: PathBuf,
    pub input: PathBuf,
    pub work: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// 폴더 이름에 쓸 수 있는 글자만 남긴다.
fn safe(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn make_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// 이미 있으면 `AlreadyExists`로 실패하는 0700 폴더 만들기(상위 폴더는 만들지 않는다).
fn make_exclusive(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// 폴더 이름 끝에 붙일 무작위 낱말(`/dev/urandom`, 없으면 시각+순번). 겹치면 배타적 생성이 걸러 다시 한다.
fn random_suffix() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    let read = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if read.is_err() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mixed = now ^ u128::from(NEXT.fetch_add(1, Ordering::Relaxed));
        bytes.copy_from_slice(&mixed.to_le_bytes()[..8]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl CallDir {
    fn create(workspace: &Path, attempt: &Attempt) -> io::Result<CallDir> {
        Self::create_with(workspace, attempt, random_suffix)
    }

    /// 이름의 끝 낱말을 `suffix`가 정한다(겹치는 이름을 시험하려고 바꿀 수 있다). 이미 있는 폴더는 건드리지 않고 다음 낱말로 다시 한다.
    fn create_with(
        workspace: &Path,
        attempt: &Attempt,
        mut suffix: impl FnMut() -> String,
    ) -> io::Result<CallDir> {
        make_dir(workspace)?;
        for _ in 0..16 {
            let name = format!(
                "{}-{}-g{}-c{}-{}",
                safe(&attempt.profile.0),
                safe(&attempt.run.0),
                attempt.generation.0,
                attempt.claim.map_or(0, |c| c.0),
                suffix()
            );
            let root = workspace.join(name);
            match make_exclusive(&root) {
                Ok(()) => {
                    let (input, work) = (root.join("in"), root.join("work"));
                    let dir = CallDir { root, input, work };
                    // 하나라도 못 만들면 만든 것을 지우고 실패한다(`Drop`)
                    make_exclusive(&dir.input)?;
                    make_exclusive(&dir.work)?;
                    return Ok(dir);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::from(io::ErrorKind::AlreadyExists))
    }

    /// 입력 폴더 안에 파일을 쓴다(상위 폴더는 만든다). 경로는 이 크레이트가 고정한 상대 경로뿐이다.
    pub(crate) fn write_input(&self, relative: &str, content: &str) -> io::Result<()> {
        write_under(&self.input, relative, content)
    }

    pub(crate) fn write_work(&self, relative: &str, content: &str) -> io::Result<()> {
        write_under(&self.work, relative, content)
    }
}

fn write_under(base: &Path, relative: &str, content: &str) -> io::Result<()> {
    let path = base.join(relative);
    if let Some(parent) = path.parent() {
        make_dir(parent)?;
    }
    fs::write(path, content)
}

impl Drop for CallDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn prepare_failed() -> ModelError {
    ModelError::Failed {
        code: "prepare_failed",
        transient: true,
    }
}

pub(crate) fn failed(code: &'static str) -> ModelError {
    ModelError::Failed {
        code,
        transient: false,
    }
}

/// 모델 별칭을 풀고 프런트 채널과 내장 도구 요구를 확인한 뒤 호출 폴더를 만들어 지침(`AGENTS.md`)을 쓴다. 채널 없이는 CLI가 모델에 닿지 못한다.
pub(crate) fn prepare(
    config: &CliConfig,
    request: &ModelRequest,
) -> Result<(CliModel, CallDir, RelayChannel), ModelError> {
    let relay = request.relay.clone().ok_or(ModelError::Failed {
        code: "relay_unavailable",
        transient: true,
    })?;
    relay.validate()?;
    if !request.builtin.is_empty() && !config.confined_reads {
        return Err(failed("builtin_unconfined"));
    }
    let model = config
        .models
        .get(&request.model)
        .cloned()
        .ok_or(ModelError::Failed {
            code: "unknown_model",
            transient: false,
        })?;
    let call =
        CallDir::create(&config.workspace, &request.attempt).map_err(|_| prepare_failed())?;
    call.write_input("AGENTS.md", &request.system)
        .map_err(|_| prepare_failed())?;
    Ok((model, call, relay))
}

/// CLI가 낸 결과는 항상 최종 답이다(도구 순환은 CLI 안에서 끝났다).
pub(crate) fn final_step(output: Output) -> ModelStep {
    ModelStep::Final {
        text: output.text,
        usage: output.usage,
    }
}

#[cfg(test)]
mod tests;
