//! Read-only CLI commands backed by `grove-core` public views.

use grove_core::query;
use grove_core::state;
use grove_core::{Paths, TmuxServer};
use grove_core::{ipc, protocol};

pub fn run(args: &[String], paths: &Paths) -> Result<(), Box<dyn std::error::Error>> {
    let method = query_method(args)?;

    // Prefer the persistent service so multiple clients share one owner
    // boundary. A missing service is normal for read-only commands, which
    // retain their original direct collection path.
    if ipc::send_command(&paths.notify_socket(), &ipc::Command::Ping)? {
        let request = protocol::Request::new(
            format!("cli-{}", std::process::id()),
            method,
            serde_json::json!({}),
        );
        // A service from an older Grove release may still own the socket
        // across an upgrade but not understand framed requests. Any transport
        // failure falls through to direct collection; a structured response
        // from a current service remains authoritative.
        if let Ok(response) = protocol::call(&paths.notify_socket(), &request) {
            let value = service_result(method, response)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
    }

    let state = state::load(&paths.state_file())?;
    // Listing an existing server does not need its startup config. Leaving it
    // off is what keeps these commands genuinely read-only: `with_config`
    // would ensure the config files exist before invoking tmux.
    let server = TmuxServer::new(paths.tmux_socket());

    let value = match method {
        "project.list" => serde_json::to_value(query::list_projects(&state))?,
        "worktree.list" => serde_json::to_value(query::list_worktrees(&state, &server)?)?,
        "session.list" => serde_json::to_value(query::list_sessions(&server)?)?,
        "session.snapshot" => serde_json::to_value(query::snapshot(&state, &server)?)?,
        _ => return Err(usage_error(args)),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn query_method(args: &[String]) -> Result<&'static str, Box<dyn std::error::Error>> {
    match args {
        [resource, action] if action == "list" => match resource.as_str() {
            "project" => Ok("project.list"),
            "worktree" => Ok("worktree.list"),
            "session" => Ok("session.list"),
            _ => Err(usage_error(args)),
        },
        [command] if command == "snapshot" => Ok("session.snapshot"),
        _ => Err(usage_error(args)),
    }
}

fn service_result(
    method: &str,
    response: protocol::Response,
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

fn usage_error(args: &[String]) -> Box<dyn std::error::Error> {
    let command = args.join(" ");
    format!(
        "unknown query command `{command}`; expected `snapshot`, `project list`, `worktree list`, or `session list`"
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
    fn query_methods_accept_only_the_documented_commands() {
        for (args, expected) in [
            (strings(&["project", "list"]), "project.list"),
            (strings(&["worktree", "list"]), "worktree.list"),
            (strings(&["session", "list"]), "session.list"),
            (strings(&["snapshot"]), "session.snapshot"),
        ] {
            assert_eq!(query_method(&args).expect("documented query"), expected);
        }

        for args in [
            Vec::new(),
            strings(&["project"]),
            strings(&["project", "show"]),
            strings(&["unknown", "list"]),
            strings(&["snapshot", "extra"]),
        ] {
            let error = query_method(&args).expect_err("unknown query");
            assert!(error.to_string().contains("unknown query command"));
        }
    }

    #[test]
    fn service_results_are_strict_and_preserve_rejections() {
        let value = serde_json::json!({"version": 1, "projects": []});
        assert_eq!(
            service_result(
                "project.list",
                protocol::Response::success("query", value.clone())
            )
            .expect("success"),
            value
        );

        let rejection = service_result(
            "project.list",
            protocol::Response::error("query", "state_read_failed", "corrupt state"),
        )
        .expect_err("rejection");
        assert!(
            rejection
                .to_string()
                .contains("state_read_failed: corrupt state")
        );

        for response in [
            protocol::Response {
                protocol: protocol::VERSION,
                id: "query".into(),
                ok: true,
                result: None,
                error: None,
            },
            protocol::Response {
                protocol: protocol::VERSION,
                id: "query".into(),
                ok: false,
                result: Some(serde_json::json!({})),
                error: None,
            },
            protocol::Response {
                protocol: protocol::VERSION,
                id: "query".into(),
                ok: true,
                result: Some(serde_json::json!({})),
                error: Some(protocol::ResponseError {
                    code: "contradiction".into(),
                    message: "both".into(),
                }),
            },
        ] {
            assert!(
                service_result("project.list", response)
                    .expect_err("ambiguous")
                    .to_string()
                    .contains("invalid response")
            );
        }
    }

    #[test]
    fn usage_errors_name_the_bad_command_and_every_valid_shape() {
        let error = usage_error(&strings(&["project", "show"])).to_string();
        assert!(error.contains("project show"));
        for expected in ["snapshot", "project list", "worktree list", "session list"] {
            assert!(error.contains(expected));
        }
    }
}
