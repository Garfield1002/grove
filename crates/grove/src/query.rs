//! Read-only CLI commands backed by `grove-core` public views.

use grove_core::query;
use grove_core::state;
use grove_core::{Paths, TmuxServer};
use grove_core::{ipc, protocol};

pub fn run(args: &[String], paths: &Paths) -> Result<(), Box<dyn std::error::Error>> {
    let method = match args {
        [resource, action] if action == "list" => match resource.as_str() {
            "project" => "project.list",
            "worktree" => "worktree.list",
            "session" => "session.list",
            _ => return Err(usage_error(args)),
        },
        [command] if command == "snapshot" => "session.snapshot",
        _ => return Err(usage_error(args)),
    };

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
            let value = match (response.result, response.error) {
                (Some(result), None) if response.ok => result,
                (None, Some(error)) if !response.ok => {
                    return Err(format!(
                        "Grove service rejected `{method}`: {}: {}",
                        error.code, error.message
                    )
                    .into());
                }
                _ => return Err("Grove service returned an invalid response".into()),
            };
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

fn usage_error(args: &[String]) -> Box<dyn std::error::Error> {
    let command = args.join(" ");
    format!(
        "unknown query command `{command}`; expected `snapshot`, `project list`, `worktree list`, or `session list`"
    )
    .into()
}
