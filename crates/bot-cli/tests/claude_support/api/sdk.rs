//! 공식 SDK 시험(파이썬 unittest·Node `node:test`)을 시험 프로세스 밖에서 돌리는 도구. 환경은 지운 채 필요한 값만 준다 — 호스트의 `OPENAI_*`·프록시 환경이 새지 않고,
//! SDK는 `base_url`·`api_key`만 바뀐 채 루프백의 와이어 프록시(실제로 간 요청을 세고 모든 응답을 공식 OpenAPI로 검사한다)를 거쳐 제품 입구에 닿는다. 실제 OpenAI API·실키는 쓰지 않는다.
use super::Fixture;
use crate::process_support::{http_get, which};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

pub struct Sdk {
    dir: PathBuf,
    src: PathBuf,
    python: PathBuf,
    node: PathBuf,
}

/// 시험 한 번의 결과. `ok`는 종료 코드 0·실패 0·건너뜀 0·시험이 하나 이상 돌았을 때만 참이다.
pub struct Outcome {
    pub ok: bool,
    pub ran: usize,
    pub log: String,
}

/// 위치 문제를 필수 모드에서는 실패로, 아니면 건너뜀 표지를 남기고 None으로 돌려준다.
fn unavailable(why: &str) -> Option<Sdk> {
    assert!(
        std::env::var("BOT_OPENAI_CANARY").as_deref() != Ok("required"),
        "BOT_OPENAI_CANARY=required: {why}"
    );
    eprintln!("skipped: {why}");
    None
}

impl Sdk {
    /// `BOT_OPENAI_SDK_DIR`(`scripts/openai-sdk-setup.sh`가 만든 폴더)가 가리키는 환경. 없으면 건너뛴다(필수 모드에서는 실패). 있는데 어긋나면 실패한다.
    pub fn locate() -> Option<Self> {
        let Some(dir) = std::env::var_os("BOT_OPENAI_SDK_DIR").map(PathBuf::from) else {
            return unavailable(
                "BOT_OPENAI_SDK_DIR가 없어 공식 SDK 시험을 건너뜀(scripts/openai-sdk-setup.sh)",
            );
        };
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/openai_sdk");
        let python = dir.join("venv/bin/python");
        let Some(node) = which("node") else {
            return unavailable("node가 없어 공식 SDK 시험을 건너뜀");
        };
        assert!(
            python.is_file() && dir.join("node/node_modules/openai/package.json").is_file(),
            "{}: 공식 SDK 환경이 불완전하다 — scripts/openai-sdk-setup.sh를 다시 돌려라",
            dir.display()
        );
        // 설치 폴더의 Node 시험 파일은 저장소 것의 복사본이다: 어긋나면 옛 시험이 돈다
        for entry in std::fs::read_dir(src.join("node")).unwrap().flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".mjs") {
                assert_eq!(
                    std::fs::read(entry.path()).unwrap(),
                    std::fs::read(dir.join("node").join(&name)).unwrap_or_default(),
                    "{}: 설치 폴더의 Node 시험 파일이 저장소와 다르다 — scripts/openai-sdk-setup.sh를 다시 돌려라",
                    name.to_string_lossy()
                );
            }
        }
        Some(Self {
            dir,
            src,
            python,
            node,
        })
    }

    /// 와이어 프록시를 띄운다(입구 포트로 전달한다). 버리면 멈춘다.
    pub async fn proxy(&self, gateway_port: u16) -> Proxy {
        let mut child = Command::new(&self.python)
            .arg("-I")
            .arg(self.src.join("wire_proxy.py"))
            .arg(gateway_port.to_string())
            .env_clear()
            .env("WIRE_VALIDATE", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("와이어 프록시 시작");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("프록시가 포트를 알리지 않았다")
            .unwrap()
            .expect("프록시가 바로 끝났다");
        Proxy {
            port: first.trim().parse().expect("프록시 포트"),
            _child: child,
        }
    }

    /// SDK 시험 프로세스에 줄 환경(두 SDK 시험이 같이 쓴다)
    pub fn env(&self, fixture: &Fixture, proxy: &Proxy) -> Vec<(String, String)> {
        let pair = |k: &str, v: String| (k.to_owned(), v);
        vec![
            pair(
                "OPENAI_BASE_URL",
                format!("http://127.0.0.1:{}/v1", proxy.port),
            ),
            pair(
                "OPENAI_WIRE_URL",
                format!("http://127.0.0.1:{}/__wire", proxy.port),
            ),
            pair("OPENAI_E2E_CONTROL", fixture.control.url()),
            pair("OPENAI_API_KEY", fixture.keys.build.clone()),
            pair("OPENAI_ROTATED_KEY", fixture.keys.rotated.clone()),
            pair("OPENAI_OTHER_KEY", fixture.keys.ops.clone()),
            pair("OPENAI_GUEST_KEY", fixture.keys.guest.clone()),
            pair("OPENAI_WAIT_MS", fixture.shape.wait_ms().to_string()),
        ]
    }

    fn command(&self, program: &Path, env: &[(String, String)]) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// `sdk_run.py`를 돌린다: 시험은 그 안에서 돌고 검사된다 — `expected.json`의 시험이 전부 통과해야 하고, 건너뜀·xfail·todo·누락·미등록·통과 0건은 실패다. 결과 한 줄(JSON)에서 돌린 수를 읽는다.
    async fn run_checked(&self, env: &[(String, String)], args: &[&str]) -> Outcome {
        let output = self
            .command(&self.python, env)
            .arg("-I")
            .arg(self.src.join("sdk_run.py"))
            .args(args)
            .output()
            .await
            .expect("sdk_run.py 실행");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let result: Value = stdout
            .lines()
            .last()
            .and_then(|line| serde_json::from_str(line).ok())
            .unwrap_or(Value::Null);
        let log = format!("{}{}", stdout, String::from_utf8_lossy(&output.stderr));
        let ran = result["ran"].as_u64().unwrap_or(0) as usize;
        Outcome {
            ok: output.status.success() && result["ok"] == true && ran > 0,
            ran,
            log,
        }
    }

    /// 파이썬 시험 파일(예: `test_models.py`)을 돌린다.
    pub async fn python(&self, env: &[(String, String)], file: &str) -> Outcome {
        self.run_checked(env, &["py", file]).await
    }

    /// Node 시험 파일(설치 폴더의 `node/<file>`)을 돌린다.
    pub async fn node(&self, env: &[(String, String)], file: &str) -> Outcome {
        let dir = self.dir.join("node");
        self.run_checked(env, &["node", file, "--node-dir", dir.to_str().unwrap()])
            .await
    }

    /// `expected.json`의 한 묶음(`main`·`tight`)에서 기대하는 시험 파일들(파이썬·Node)
    pub fn suite(&self, profile: &str) -> (Vec<String>, Vec<String>) {
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(self.src.join("expected.json")).unwrap())
                .expect("expected.json은 JSON");
        let files = |kind: &str| -> Vec<String> {
            expected["suites"][profile][kind]
                .as_array()
                .unwrap_or_else(|| panic!("expected.json에 {profile}.{kind}가 없다"))
                .iter()
                .map(|f| f.as_str().unwrap().to_owned())
                .collect()
        };
        (files("python"), files("node"))
    }

    /// 프록시가 실제로 본 것을 검사한다: 공식 스키마·SSE 틀 위반 0, 요청 1건 이상, 기대한 공식 스키마를 모두 한 번 이상 검사했다.
    pub async fn check_proxy(
        &self,
        env: &[(String, String)],
        proxy: &Proxy,
        profile: &str,
    ) -> Outcome {
        let url = format!("http://127.0.0.1:{}/__wire", proxy.port);
        self.run_checked(env, &["proxy", "--url", &url, "--profile", profile])
            .await
    }
}

/// 와이어 프록시: 실제로 입구에 간 요청과 응답 검사 결과를 알려 준다.
pub struct Proxy {
    pub port: u16,
    _child: Child,
}

impl Proxy {
    async fn get(&self, path: &'static str) -> Value {
        let addr = format!("127.0.0.1:{}", self.port);
        let (status, body) = tokio::task::spawn_blocking(move || http_get(&addr, path, None))
            .await
            .unwrap();
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).expect("프록시 응답은 JSON")
    }

    /// 공식 스키마·SSE 틀 위반 목록과 검사한 응답 수(스키마별), 상태 코드별 응답 수, 보낸 요청 수
    pub async fn summary(&self) -> Value {
        self.get("/__wire/summary").await
    }
}
