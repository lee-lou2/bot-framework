//! CLI 한 번 실행: 샌드박스에서 프로세스를 띄워 출력을 줄 단위로 `Parser`에 먹이고, 전체 시간·무출력·취소를 지킨다.
//! 성공·실패는 종료 코드가 아니라 CLI가 낸 마지막 결과로 가른다. stderr는 결과 없이 죽었을 때 **분류**에만 쓰고 밖으로 내보내지 않는다.

use super::{Limits, ModelError, Usage};
use bot_core::{
    command::RunOutcome,
    sandbox::{Exit, ModelProcess, ModelSandbox, ModelSpec},
};
use tokio::{sync::watch, time::Instant};

/// 진행 신호. 출력이 있다는 뜻이다(무출력 시간 재기는 줄이 올 때마다 새로 시작한다).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Thinking,
    Writing,
    /// 재시도 중
    Retrying,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Output {
    pub text: String,
    pub usage: Usage,
    /// 실행기가 아는 계량(`ModelStep::Final::outcome`).
    pub outcome: RunOutcome,
}

/// CLI의 출력을 한 줄씩 읽어 진행 신호와 마지막 결과를 만든다. 순수.
pub trait Parser: Send {
    fn feed(&mut self, line: &str) -> Option<Event>;
    /// CLI가 끝까지 결과를 냈는가
    fn has_result(&self) -> bool;
    fn finish(self: Box<Self>) -> Result<Output, ModelError>;
}

/// 잠시 뒤 다시 하면 풀릴 만한 오류 문구인가 (프록시 과부하 · 한도 · 네트워크).
pub fn is_transient(message: &str) -> bool {
    classify(message, None).1
}

/// 오류 문구와 HTTP 상태에서 (고정 코드, 일시 여부)를 가린다. 문구 자체는 돌려주지 않는다.
pub fn classify(message: &str, status: Option<u64>) -> (&'static str, bool) {
    let m = message.to_lowercase();
    // 공급자의 출력 토큰 상한에서 답이 끊겼다(CLI는 부분 글을 결과로 내지 않고 이 오류로 끝낸다): 다시 해도 같다. 잘린 글을 성공으로 올리지 않는다
    if m.contains("output token maximum") {
        return ("output_limit", false);
    }
    if status == Some(429) || m.contains("429") || m.contains("rate limit") {
        return ("rate_limited", true);
    }
    let overloaded = matches!(status, Some(500 | 502 | 503 | 504 | 529))
        || ["503", "502", "504", "529", "overloaded", "temporarily"]
            .iter()
            .any(|k| m.contains(k));
    if overloaded {
        return ("overloaded", true);
    }
    if ["timed out", "econnreset", "socket hang up"]
        .iter()
        .any(|k| m.contains(k))
    {
        return ("network", true);
    }
    ("model_error", false)
}

/// 취소 신호가 `true`가 되면 돌아온다. 송신 쪽이 사라지면 취소가 아니므로 영원히 기다린다.
pub(super) async fn cancelled(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn halt(process: &mut Box<dyn ModelProcess>, error: ModelError) -> ModelError {
    process.kill().await;
    error
}

/// 모델 CLI를 한 번 돌려 결과를 받는다.
pub async fn drive(
    sandbox: &ModelSandbox,
    spec: ModelSpec,
    parser: Box<dyn Parser>,
    limits: Limits,
    cancel: watch::Receiver<bool>,
) -> Result<Output, ModelError> {
    // 송신 쪽이 없는 수신기는 영원히 신호가 오지 않는다
    drive_with(
        sandbox,
        spec,
        parser,
        limits,
        cancel,
        watch::channel(false).1,
    )
    .await
}

/// `drive`에 호스트 쪽 중단 신호(`abort`)를 더한 것: `true`가 되면 프로세스를 끝내고 `Cancelled`를 돌려준다(세션 관찰자가 이 실행을
/// 더 믿을 수 없을 때 쓴다. 호출한 쪽이 자기 사유로 오류를 바꿔 돌려준다).
pub async fn drive_with(
    sandbox: &ModelSandbox,
    spec: ModelSpec,
    mut parser: Box<dyn Parser>,
    limits: Limits,
    mut cancel: watch::Receiver<bool>,
    mut abort: watch::Receiver<bool>,
) -> Result<Output, ModelError> {
    if *cancel.borrow() {
        return Err(ModelError::Cancelled);
    }
    // 띄우는 동안에도 취소·중단을 지킨다: 점검이 느리거나 VM이 뜨는 중이어도 종료 반납 신호가 실행기를 끊는다. 취소가 이기면 시작 future를 버린다
    // (`Sandbox::spawn`은 버려도 아무것도 남기지 않는다). 프로세스를 이미 받았다면 아래 루프의 첫 `select!`가 곧바로 끝낸다(`kill`이 컨테이너가 사라진 것까지 확인한다).
    let mut process = tokio::select! {
        biased;
        () = cancelled(&mut cancel) => return Err(ModelError::Cancelled),
        () = cancelled(&mut abort) => return Err(ModelError::Cancelled),
        spawned = sandbox.spawn(spec) => spawned.map_err(|_| ModelError::Failed {
            code: "sandbox_failed",
            transient: true,
        })?,
    };
    let deadline = Instant::now() + limits.total;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(halt(&mut process, ModelError::Timeout).await);
        }
        let wait = limits.idle.min(remaining);
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Err(halt(&mut process, ModelError::Cancelled).await),
            _ = cancelled(&mut abort) => return Err(halt(&mut process, ModelError::Cancelled).await),
            line = tokio::time::timeout(wait, process.next_line()) => match line {
                Ok(Some(line)) => {
                    parser.feed(&line);
                }
                Ok(None) => break,
                Err(_) => {
                    let error = if Instant::now() >= deadline {
                        ModelError::Timeout
                    } else {
                        ModelError::Failed { code: "idle_timeout", transient: true }
                    };
                    return Err(halt(&mut process, error).await);
                }
            },
        }
    }
    // 출력이 닫혀도 프로세스가 끝났다는 뜻은 아니다 — 끝날 때까지도 전체 시간과 취소를 지킨다.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let exit: Exit = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => return Err(halt(&mut process, ModelError::Cancelled).await),
        _ = cancelled(&mut abort) => return Err(halt(&mut process, ModelError::Cancelled).await),
        exit = tokio::time::timeout(remaining, process.wait()) => match exit {
            Ok(exit) => exit,
            Err(_) => return Err(halt(&mut process, ModelError::Timeout).await),
        },
    };
    if parser.has_result() {
        return parser.finish();
    }
    // 결과 없이 끝났다: stderr는 분류에만 쓴다(일시 오류면 다시 해 볼 만하다).
    Err(match classify(&exit.stderr_tail, None) {
        (code, true) => ModelError::Failed {
            code,
            transient: true,
        },
        _ => ModelError::Failed {
            code: "no_result",
            transient: false,
        },
    })
}

#[cfg(test)]
mod tests;
