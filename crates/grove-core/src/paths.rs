//! XDG base directory resolution.
//!
//! Resolution is a pure function of the environment (`Env`), so tests can
//! exercise every fallback without mutating process-global state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Application directory name under each XDG base directory.
pub const APP_DIR: &str = "grove";
/// File name of the private tmux socket.
pub const SOCKET_FILE: &str = "tmux.sock";

/// The environment variables path resolution depends on.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<OsString>,
    pub config_home: Option<OsString>,
    pub state_home: Option<OsString>,
    pub runtime_dir: Option<OsString>,
    pub user: Option<OsString>,
    pub tmp_dir: PathBuf,
}

impl Env {
    /// Read the environment of the current process.
    pub fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            config_home: std::env::var_os("XDG_CONFIG_HOME"),
            state_home: std::env::var_os("XDG_STATE_HOME"),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            user: std::env::var_os("USER"),
            tmp_dir: std::env::temp_dir(),
        }
    }
}

fn absolute(value: &Option<OsString>) -> Option<PathBuf> {
    let value = value.as_ref()?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    // The XDG spec says relative values must be ignored.
    path.is_absolute().then_some(path)
}

/// Resolved locations of everything Grove reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    /// Resolve from an environment description.
    ///
    /// - config: `$XDG_CONFIG_HOME/grove`, else `$HOME/.config/grove`
    /// - state: `$XDG_STATE_HOME/grove`, else `$HOME/.local/state/grove`
    /// - runtime: `$XDG_RUNTIME_DIR/grove`, else `<tmp>/grove-<user>`
    pub fn resolve(env: &Env) -> Result<Self> {
        let config_dir = match absolute(&env.config_home) {
            Some(base) => base.join(APP_DIR),
            None => home(env, "XDG_CONFIG_HOME")?.join(".config").join(APP_DIR),
        };
        let state_dir = match absolute(&env.state_home) {
            Some(base) => base.join(APP_DIR),
            None => home(env, "XDG_STATE_HOME")?
                .join(".local")
                .join("state")
                .join(APP_DIR),
        };
        let runtime_dir = match absolute(&env.runtime_dir) {
            Some(base) => base.join(APP_DIR),
            None => {
                let user = env
                    .user
                    .as_ref()
                    .map(|u| u.to_string_lossy().into_owned())
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| "user".to_string());
                env.tmp_dir.join(format!("{APP_DIR}-{user}"))
            }
        };
        Ok(Self {
            config_dir,
            state_dir,
            runtime_dir,
        })
    }

    /// Resolve from the current process environment.
    pub fn from_process_env() -> Result<Self> {
        Self::resolve(&Env::from_process())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.toml")
    }

    /// The private tmux socket. Never the user's default server.
    pub fn tmux_socket(&self) -> PathBuf {
        self.runtime_dir.join(SOCKET_FILE)
    }

    /// The socket `grove notify` writes to, beside the tmux socket in the
    /// runtime directory so both die with the login session.
    pub fn notify_socket(&self) -> PathBuf {
        crate::ipc::socket_path(&self.runtime_dir)
    }

    /// Grove's own tmux configuration, passed as `-f`. Without it a private
    /// server would still read `~/.tmux.conf`.
    pub fn tmux_config_file(&self) -> PathBuf {
        self.config_dir.join("tmux.conf")
    }

    /// The Grove-owned half of the tmux configuration, sourced by
    /// `tmux.conf`. Rewritten from the binary on every start, so fixes ship
    /// with an upgrade instead of being frozen into the user's copy.
    pub fn managed_tmux_config_file(&self) -> PathBuf {
        self.config_dir.join("grove.tmux.conf")
    }
}

fn home(env: &Env, wanted: &'static str) -> Result<PathBuf> {
    absolute(&env.home).ok_or(Error::NoHomeDirectory(wanted))
}

/// Create a directory (and parents) if missing, then make sure it is only
/// accessible to its owner. The tmux socket directory in particular must be
/// 0700.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::io(format!("could not create {}", dir.display()), e))?;
    }
    let metadata = std::fs::metadata(dir)
        .map_err(|e| Error::io(format!("could not stat {}", dir.display()), e))?;
    if !metadata.is_dir() {
        return Err(Error::io(
            format!("{} exists but is not a directory", dir.display()),
            std::io::Error::from(std::io::ErrorKind::NotADirectory),
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::io(format!("could not secure {}", dir.display()), e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn env() -> Env {
        Env {
            home: Some("/home/u".into()),
            tmp_dir: PathBuf::from("/tmp"),
            ..Env::default()
        }
    }

    #[test]
    fn xdg_variables_win() {
        let paths = Paths::resolve(&Env {
            config_home: Some("/x/config".into()),
            state_home: Some("/x/state".into()),
            runtime_dir: Some("/run/user/1000".into()),
            ..env()
        })
        .expect("resolves");
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/x/config/grove/config.toml")
        );
        assert_eq!(
            paths.state_file(),
            PathBuf::from("/x/state/grove/state.toml")
        );
        assert_eq!(
            paths.tmux_socket(),
            PathBuf::from("/run/user/1000/grove/tmux.sock")
        );
        assert_eq!(
            paths.notify_socket(),
            PathBuf::from("/run/user/1000/grove/notify.sock")
        );
        assert_eq!(
            paths.tmux_config_file(),
            PathBuf::from("/x/config/grove/tmux.conf")
        );
    }

    #[test]
    fn the_tmux_config_lives_beside_config_toml() {
        let paths = Paths::resolve(&env()).expect("resolves");
        assert_eq!(
            paths.tmux_config_file(),
            PathBuf::from("/home/u/.config/grove/tmux.conf")
        );
        assert_eq!(
            paths.tmux_config_file().parent(),
            paths.config_file().parent()
        );
    }

    #[test]
    fn falls_back_to_home() {
        let paths = Paths::resolve(&env()).expect("resolves");
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/u/.config/grove/config.toml")
        );
        assert_eq!(
            paths.state_file(),
            PathBuf::from("/home/u/.local/state/grove/state.toml")
        );
    }

    #[test]
    fn runtime_dir_falls_back_to_tmp_when_unset() {
        let paths = Paths::resolve(&Env {
            user: Some("jack".into()),
            ..env()
        })
        .expect("resolves");
        assert_eq!(
            paths.tmux_socket(),
            PathBuf::from("/tmp/grove-jack/tmux.sock")
        );
    }

    #[test]
    fn runtime_dir_fallback_without_user_is_still_usable() {
        let paths = Paths::resolve(&env()).expect("resolves");
        assert_eq!(
            paths.tmux_socket(),
            PathBuf::from("/tmp/grove-user/tmux.sock")
        );
    }

    #[test]
    fn relative_and_empty_xdg_values_are_ignored() {
        let paths = Paths::resolve(&Env {
            config_home: Some("relative/path".into()),
            state_home: Some("".into()),
            runtime_dir: Some("also/relative".into()),
            user: Some("jack".into()),
            ..env()
        })
        .expect("resolves");
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/u/.config/grove/config.toml")
        );
        assert_eq!(
            paths.state_file(),
            PathBuf::from("/home/u/.local/state/grove/state.toml")
        );
        assert_eq!(
            paths.tmux_socket(),
            PathBuf::from("/tmp/grove-jack/tmux.sock")
        );
    }

    #[test]
    fn without_home_or_xdg_config_it_is_an_error() {
        let err = Paths::resolve(&Env::default()).expect_err("no home");
        assert!(matches!(err, Error::NoHomeDirectory("XDG_CONFIG_HOME")));
    }

    #[test]
    fn ensure_private_dir_creates_0700() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("grove");
        ensure_private_dir(&dir).expect("creates");
        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn ensure_private_dir_tightens_existing_permissions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("grove");
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        ensure_private_dir(&dir).expect("tightens");
        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn ensure_private_dir_rejects_a_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("grove");
        std::fs::write(&file, b"not a dir").expect("write");
        assert!(ensure_private_dir(&file).is_err());
    }
}
