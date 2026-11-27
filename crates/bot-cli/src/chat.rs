//! Local CLI chat command (`bot chat`) over the user's Unix socket.
use crate::{cli::ChatArgs, load::load_host, settings::Settings, term::plain};
use bot_gateway::cli::{
    client::{self, Chat, Outcome, new_request_id, render},
    socket::{current_uid, socket_path},
};
use serde_json::Value;
use std::process::ExitCode;

/// Exit code for retryable failures (store outage, rate limit, or timeout).
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
        return Err(
            "--request-id must be 1..=128 characters of ASCII alphanumerics and . _ : -".into(),
        );
    }
    let on_event = |event: &Value| match event["type"].as_str() {
        Some("accepted") => eprintln!("Accepted (request ID: {request})"),
        Some("state") if event["state"] == "waiting" => {
            eprintln!("... waiting for approval or child run (approve in the Admin UI)");
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
            .ok_or("message text is required (e.g. bot chat \"Show today's agenda\")")?;
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
                "could not connect to server — check that `bot serve` is running (socket: {})",
                socket.display()
            ))
        }
        // Peer or socket directory ownership check failed before sending; do not signal retryable.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "{error} — message was not sent (socket: {})",
            socket.display()
        )),
        Err(error) => {
            eprintln!(
                "Connection lost ({error}). Reattach with the same request ID: bot chat --attach --request-id {request}"
            );
            Ok(ExitCode::from(RETRYABLE))
        }
        Ok(Outcome::Accepted) => {
            println!("{request}");
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Done { state, result }) => {
            if state == "succeeded" {
                // Sanitize control characters from untrusted model/tool output before printing to terminal.
                let text = result.as_ref().map(render).unwrap_or_default();
                if !text.is_empty() {
                    println!("{}", plain(&text));
                }
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!(
                    "Run {} (request ID: {request})",
                    if state == "cancelled" {
                        "was cancelled"
                    } else {
                        "failed"
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
                    .map(|s| format!("after {s}s, "))
                    .unwrap_or_default();
                eprintln!(
                    "Retry {wait}with the same request ID: bot chat --request-id {request} ..."
                );
                Ok(ExitCode::from(RETRYABLE))
            } else {
                Ok(ExitCode::FAILURE)
            }
        }
    }
}
