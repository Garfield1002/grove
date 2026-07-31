//! Headless control commands backed by Grove's persistent service.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use grove_core::Paths;
use grove_core::protocol::{self, EventKind, Request, Response};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

pub fn run(args: &[String], paths: &Paths) -> Result<(), Box<dyn std::error::Error>> {
    crate::service::ensure_running(paths)?;
    match args {
        [resource, action, worktree_id, rest @ ..]
            if resource == "session" && matches!(action.as_str(), "ensure" | "open") =>
        {
            mutate(
                paths,
                &format!("session.{action}"),
                worktree_id,
                idempotency_key(rest)?,
            )
        }
        [resource, action, worktree_id, rest @ ..] if resource == "agent" && action == "start" => {
            mutate(paths, "agent.start", worktree_id, idempotency_key(rest)?)
        }
        [command, worktree_id, rest @ ..] if command == "wait" => {
            let (status, timeout) = wait_options(rest)?;
            wait(paths, worktree_id, &status, timeout)
        }
        _ => Err(usage_error(args)),
    }
}

fn mutate(
    paths: &Paths,
    method: &str,
    worktree_id: &str,
    idempotency_key: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = call(
        paths,
        &Request::new(
            request_id("control"),
            method,
            serde_json::json!({
                "worktree_id": worktree_id,
                "idempotency_key": idempotency_key,
            }),
        ),
        CONTROL_TIMEOUT,
    )?;
    print_result(method, response)
}

fn wait(
    paths: &Paths,
    worktree_id: &str,
    desired: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = Request::new(
        request_id("wait-events"),
        "event.subscribe",
        serde_json::json!({
            "topics": [
                EventKind::StateChanged,
                EventKind::NotificationReceived,
                EventKind::ControlCompleted,
            ]
        }),
    );
    let (mut stream, response) = open_subscription(paths, &request)?;
    if !response.ok {
        return Err(response_error("event.subscribe", response));
    }
    let (events, wake) = mpsc::channel();
    std::thread::Builder::new()
        .name("grove-wait-events".into())
        .spawn(move || {
            while protocol::read_event(&mut stream).is_ok() {
                if events.send(()).is_err() {
                    break;
                }
            }
        })?;

    let started = Instant::now();
    loop {
        let response = call(
            paths,
            &Request::new(
                request_id("wait-status"),
                "status.get",
                serde_json::json!({"worktree_id": worktree_id}),
            ),
            CONTROL_TIMEOUT,
        )?;
        let result = response_result("status.get", response)?;
        if result["status"].as_str() == Some(desired) {
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return Err(format!(
                "timed out after {}s waiting for `{worktree_id}` to become `{desired}`",
                timeout.as_secs()
            )
            .into());
        };
        let cadence = remaining.min(Duration::from_millis(500));
        let _ = wake.recv_timeout(cadence);
    }
}

fn call(
    paths: &Paths,
    request: &Request,
    timeout: Duration,
) -> Result<Response, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match protocol::call_with_timeout(&paths.notify_socket(), request, timeout) {
            Ok(response) => return Ok(response),
            Err(error) if service_is_starting(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn open_subscription(
    paths: &Paths,
    request: &Request,
) -> Result<(std::os::unix::net::UnixStream, Response), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match protocol::open_subscription(&paths.notify_socket(), request) {
            Ok(opened) => return Ok(opened),
            Err(error) if service_is_starting(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn service_is_starting(error: &protocol::Error) -> bool {
    matches!(
        error,
        protocol::Error::Io { context, source }
            if *context == "connect to Grove service"
                && matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
    )
}

fn response_result(
    method: &str,
    response: Response,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    match (response.result, response.error) {
        (Some(result), None) if response.ok => Ok(result),
        (None, Some(error)) if !response.ok => Err(format!(
            "Grove service rejected `{method}`: {}: {}",
            error.code, error.message
        )
        .into()),
        _ => Err("Grove service returned an invalid response".into()),
    }
}

fn response_error(method: &str, response: Response) -> Box<dyn std::error::Error> {
    match response.error {
        Some(error) => format!(
            "Grove service rejected `{method}`: {}: {}",
            error.code, error.message
        )
        .into(),
        None => "Grove service returned an invalid response".into(),
    }
}

fn print_result(method: &str, response: Response) -> Result<(), Box<dyn std::error::Error>> {
    let result = response_result(method, response)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn idempotency_key(args: &[String]) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match args {
        [] => Ok(None),
        [flag, key] if flag == "--idempotency-key" => Ok(Some(key.clone())),
        _ => Err(usage_error(args)),
    }
}

fn wait_options(args: &[String]) -> Result<(String, Duration), Box<dyn std::error::Error>> {
    let mut status = None;
    let mut timeout = DEFAULT_WAIT_TIMEOUT;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--status" if index + 1 < args.len() => {
                let value = args[index + 1].to_ascii_lowercase();
                if !matches!(
                    value.as_str(),
                    "working" | "idle" | "done" | "attention" | "stopped"
                ) {
                    return Err(format!("unknown Grove status `{value}`").into());
                }
                status = Some(value);
                index += 2;
            }
            "--timeout" if index + 1 < args.len() => {
                timeout = Duration::from_secs(args[index + 1].parse()?);
                index += 2;
            }
            _ => return Err(usage_error(args)),
        }
    }
    Ok((
        status.ok_or("wait requires `--status <working|idle|done|attention|stopped>`")?,
        timeout,
    ))
}

fn request_id(kind: &str) -> String {
    format!("cli-{kind}-{}", std::process::id())
}

fn usage_error(args: &[String]) -> Box<dyn std::error::Error> {
    format!(
        "unknown control command `{}`; expected `session ensure|open <worktree-id>`, \
         `agent start <worktree-id>`, or `wait <worktree-id> --status <status>`",
        args.join(" ")
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn idempotency_keys_accept_only_the_exact_optional_pair() {
        assert_eq!(idempotency_key(&[]).expect("optional"), None);
        assert_eq!(
            idempotency_key(&strings(&["--idempotency-key", "launch-1"])).expect("explicit key"),
            Some("launch-1".into())
        );
        for args in [
            strings(&["--idempotency-key"]),
            strings(&["--idempotency-key", "a", "b"]),
            strings(&["--other", "a"]),
        ] {
            assert!(idempotency_key(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn wait_options_are_order_independent_strict_and_case_insensitive() {
        assert_eq!(
            wait_options(&strings(&["--status", "ATTENTION", "--timeout", "7"]))
                .expect("wait options"),
            ("attention".into(), Duration::from_secs(7))
        );
        assert_eq!(
            wait_options(&strings(&["--timeout", "2", "--status", "idle"]))
                .expect("reordered options"),
            ("idle".into(), Duration::from_secs(2))
        );
        assert_eq!(
            wait_options(&strings(&["--status", "working"]))
                .expect("default timeout")
                .1,
            DEFAULT_WAIT_TIMEOUT
        );
        // The state an agent waits on most: another line of work reporting that
        // it finished.
        assert_eq!(
            wait_options(&strings(&["--status", "done"]))
                .expect("done is waitable")
                .0,
            "done"
        );

        for args in [
            Vec::new(),
            strings(&["--status"]),
            strings(&["--status", "unknown"]),
            strings(&["--timeout"]),
            strings(&["--timeout", "not-a-number", "--status", "idle"]),
            strings(&["--status", "idle", "--extra"]),
        ] {
            assert!(wait_options(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn response_decoding_rejects_every_ambiguous_shape() {
        let value = serde_json::json!({"status": "idle"});
        assert_eq!(
            response_result("status.get", Response::success("ok", value.clone())).expect("success"),
            value
        );

        let rejection = response_result(
            "status.get",
            Response::error("rejected", "unknown_worktree", "not indexed"),
        )
        .expect_err("rejection");
        assert!(
            rejection
                .to_string()
                .contains("unknown_worktree: not indexed")
        );

        for response in [
            Response {
                protocol: protocol::VERSION,
                id: "bad".into(),
                ok: true,
                result: None,
                error: None,
            },
            Response {
                protocol: protocol::VERSION,
                id: "bad".into(),
                ok: false,
                result: Some(serde_json::json!({})),
                error: None,
            },
            Response {
                protocol: protocol::VERSION,
                id: "bad".into(),
                ok: true,
                result: Some(serde_json::json!({})),
                error: Some(grove_core::protocol::ResponseError {
                    code: "contradiction".into(),
                    message: "both result and error".into(),
                }),
            },
        ] {
            assert!(
                response_result("status.get", response)
                    .expect_err("ambiguous response")
                    .to_string()
                    .contains("invalid response")
            );
        }
    }

    #[test]
    fn response_error_never_invents_success() {
        let rejected = response_error(
            "event.subscribe",
            Response::error("sub", "invalid_params", "topics required"),
        );
        assert!(
            rejected
                .to_string()
                .contains("invalid_params: topics required")
        );
        let malformed = response_error(
            "event.subscribe",
            Response::success("sub", serde_json::json!({})),
        );
        assert!(malformed.to_string().contains("invalid response"));
    }

    #[test]
    fn request_ids_and_usage_errors_are_diagnostic() {
        let id = request_id("wait-status");
        assert!(id.starts_with("cli-wait-status-"));
        assert!(id.ends_with(&std::process::id().to_string()));
        let error = usage_error(&strings(&["wrong", "shape"])).to_string();
        assert!(error.contains("wrong shape"));
        assert!(error.contains("session ensure|open"));
    }
}
