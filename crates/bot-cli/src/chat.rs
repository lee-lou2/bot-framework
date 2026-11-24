//! `bot chat` — 이 사용자의 로컬 입구(유닉스 소켓)로 에이전트와 대화한다. 요청 ID는 한 번 만들어 재시도에도 같게 쓴다:
//! 접수 직후 끊기거나 시간이 지나도 `--attach --request-id <ID>`로 같은 결과를 다시 받는다(새 접수 없음).
use crate::{cli::ChatArgs, load::load_host, settings::Settings, term::plain};
use bot_gateway::cli::{
    client::{self, Chat, Outcome, new_request_id, render},
    socket::{current_uid, socket_path},
};
use serde_json::Value;
use std::process::ExitCode;

/// 다시 시도하면 되는 실패(저장소 장애·속도 제한·기다림 초과)의 종료 코드
const RETRYABLE: u8 = 3;

pub async fn run(settings: &Settings, args: ChatArgs) -> ExitCode {
    match talk(settings, args).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("bot: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn talk(settings: &Settings, args: ChatArgs) -> Result<ExitCode, String> {
    let uid = current_uid().map_err(|e| e.to_string())?;
    // 호스트 파일이 지정돼 있으면 그 파일의 소켓 위치를 따른다(서버와 같은 규칙)
    let configured = match &settings.host_config {
        Some(_) => load_host(settings, uid)?.gateway.cli.socket,
        None => None,
    };
    let socket = socket_path(
        settings.socket.as_deref().or(configured.as_deref()),
        &|name| std::env::var(name).ok(),
        uid,
    )?;
    let request = match args.request_id.clone() {
        Some(id) => id,
        None => new_request_id().map_err(|e| e.to_string())?,
    };
    if !bot_gateway::inbound::valid_request_id(&request) {
        return Err("--request-id는 1~128자의 영숫자와 . _ : - 만 쓸 수 있어요".into());
    }
    let on_event = |event: &Value| match event["type"].as_str() {
        Some("accepted") => eprintln!("접수했어요 (요청 ID: {request})"),
        Some("state") if event["state"] == "waiting" => {
            eprintln!("… 승인 또는 하위 작업을 기다리고 있어요(관리 화면에서 승인할 수 있어요)");
        }
        _ => {}
    };
    let outcome = if args.attach {
        client::result(
            &socket,
            args.profile.as_deref(),
            &request,
            !args.no_wait,
            on_event,
        )
        .await
    } else {
        let text = args
            .text
            .clone()
            .ok_or("보낼 글이 필요해요(예: bot chat \"오늘 일정 알려줘\")")?;
        let chat = Chat {
            profile: args.profile.clone(),
            request: request.clone(),
            thread: args.thread.clone(),
            text,
            wait: !args.no_wait,
        };
        client::chat(&socket, &chat, on_event).await
    };
    match outcome {
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(format!(
                "서버에 연결하지 못했어요 — bot serve가 실행 중인지 확인하세요(소켓: {})",
                socket.display()
            ))
        }
        // 서버를 믿을 수 없다(소켓 폴더·파일이 내 것이 아니거나 반대편이 내 계정의 서버가 아니다): 대화 글을 보내지 않았다. 다시 시도해도 같으니 재시도 코드가 아니다
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "{error} — 대화 글을 보내지 않았어요(소켓: {})",
            socket.display()
        )),
        Err(error) => {
            eprintln!(
                "연결이 끊겼어요({error}). 같은 요청 ID로 결과를 다시 받을 수 있어요: bot chat --attach --request-id {request}"
            );
            Ok(ExitCode::from(RETRYABLE))
        }
        Ok(Outcome::Accepted) => {
            println!("{request}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Done { state, result }) => {
            if state == "succeeded" {
                // 모델·도구가 만든 글: 제어 문자(ESC 시퀀스 등)는 터미널에서 명령이 되니 보이는 표기로 바꿔 찍는다
                let text = result.as_ref().map(render).unwrap_or_default();
                if !text.is_empty() {
                    println!("{}", plain(&text));
                }
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!(
                    "실행이 {}로 끝났어요(요청 ID: {request})",
                    if state == "cancelled" {
                        "취소"
                    } else {
                        "실패"
                    }
                );
                Ok(ExitCode::FAILURE)
            }
        }
        Ok(Outcome::Failed {
            code,
            message,
            retry_after_secs,
        }) => {
            eprintln!("{}", plain(&message));
            let retryable = matches!(code.as_str(), "unavailable" | "rate_limited" | "timeout");
            if retryable {
                let wait = retry_after_secs
                    .map(|s| format!("{s}초 뒤에 "))
                    .unwrap_or_default();
                eprintln!(
                    "{wait}같은 요청 ID로 다시 시도하세요: bot chat --request-id {request} …"
                );
                Ok(ExitCode::from(RETRYABLE))
            } else {
                Ok(ExitCode::FAILURE)
            }
        }
    }
}
