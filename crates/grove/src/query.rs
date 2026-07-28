//! Read-only CLI commands backed by `grove-core` public views.

use grove_core::query;
use grove_core::state;
use grove_core::{Paths, TmuxServer};

pub fn run(args: &[String], paths: &Paths) -> Result<(), Box<dyn std::error::Error>> {
    let state = state::load(&paths.state_file())?;
    // Listing an existing server does not need its startup config. Leaving it
    // off is what keeps these commands genuinely read-only: `with_config`
    // would ensure the config files exist before invoking tmux.
    let server = TmuxServer::new(paths.tmux_socket());

    let value = match args {
        [resource, action] if action == "list" => match resource.as_str() {
            "project" => serde_json::to_value(query::list_projects(&state))?,
            "worktree" => serde_json::to_value(query::list_worktrees(&state, &server)?)?,
            "session" => serde_json::to_value(query::list_sessions(&server)?)?,
            _ => return Err(usage_error(args)),
        },
        _ => return Err(usage_error(args)),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn usage_error(args: &[String]) -> Box<dyn std::error::Error> {
    let command = args.join(" ");
    format!(
        "unknown query command `{command}`; expected `project list`, `worktree list`, or `session list`"
    )
    .into()
}
