//! Harness for running official Python and Node OpenAI SDK tests against the loopback wire proxy in a clean environment.
//! Clears host `OPENAI_*` and proxy vars so SDKs only hit the local OpenAPI-validating wire proxy with synthetic keys.
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

/// `ok` is true only when exit status is 0, zero failures/skips occurred, and at least one test ran.
pub struct Outcome {
    pub ok: bool,
    pub ran: usize,
    pub log: String,
}

fn unavailable(why: &str) -> Option<Sdk> {
    assert!(
        std::env::var("BOT_OPENAI_CANARY").as_deref() != Ok("required"),
        "BOT_OPENAI_CANARY=required: {why}"
    );
    eprintln!("skipped: {why}");
    None
}

impl Sdk {
    /// Locates the SDK install at `BOT_OPENAI_SDK_DIR` (built by `scripts/openai-sdk-setup.sh`).
    /// Skips if unset unless `BOT_OPENAI_CANARY=required`; panics if present but incomplete or out of sync.
    pub fn locate() -> Option<Self> {
        let Some(dir) = std::env::var_os("BOT_OPENAI_SDK_DIR").map(PathBuf::from) else {
            return unavailable(
                "BOT_OPENAI_SDK_DIR not set; skipping official SDK tests (scripts/openai-sdk-setup.sh)",
            );
        };
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/openai_sdk");
        let python = dir.join("venv/bin/python");
        let Some(node) = which("node") else {
            return unavailable("node not found; skipping official SDK tests");
        };
        assert!(
            python.is_file() && dir.join("node/node_modules/openai/package.json").is_file(),
            "{}: official SDK environment is incomplete — re-run scripts/openai-sdk-setup.sh",
            dir.display()
        );
        // Node test files in the install dir are copies from the repo; drift would run stale tests.
        for entry in std::fs::read_dir(src.join("node")).unwrap().flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".mjs") {
                assert_eq!(
                    std::fs::read(entry.path()).unwrap(),
                    std::fs::read(dir.join("node").join(&name)).unwrap_or_default(),
                    "{}: installed Node test file differs from repo — re-run scripts/openai-sdk-setup.sh",
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
            .expect("spawn wire proxy");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("wire proxy did not report port")
            .unwrap()
            .expect("wire proxy exited immediately");
        Proxy {
            port: first.trim().parse().expect("proxy port"),
            _child: child,
        }
    }

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

    /// Runs `sdk_run.py`, enforcing all manifest tests in `expected.json` pass with no skips, xfails, todos, or drift.
    async fn run_checked(&self, env: &[(String, String)], args: &[&str]) -> Outcome {
        let output = self
            .command(&self.python, env)
            .arg("-I")
            .arg(self.src.join("sdk_run.py"))
            .args(args)
            .output()
            .await
            .expect("run sdk_run.py");
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

    pub async fn python(&self, env: &[(String, String)], file: &str) -> Outcome {
        self.run_checked(env, &["py", file]).await
    }

    pub async fn node(&self, env: &[(String, String)], file: &str) -> Outcome {
        let dir = self.dir.join("node");
        self.run_checked(env, &["node", file, "--node-dir", dir.to_str().unwrap()])
            .await
    }

    pub fn suite(&self, profile: &str) -> (Vec<String>, Vec<String>) {
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(self.src.join("expected.json")).unwrap())
                .expect("expected.json must be JSON");
        let files = |kind: &str| -> Vec<String> {
            expected["suites"][profile][kind]
                .as_array()
                .unwrap_or_else(|| panic!("expected.json missing {profile}.{kind}"))
                .iter()
                .map(|f| f.as_str().unwrap().to_owned())
                .collect()
        };
        (files("python"), files("node"))
    }

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
        serde_json::from_str(&body).expect("proxy response must be JSON")
    }

    pub async fn summary(&self) -> Value {
        self.get("/__wire/summary").await
    }
}
