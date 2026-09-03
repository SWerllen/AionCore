use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

use aionui_common::{CommandSpec, EnvVar};
use sha2::{Digest, Sha256};

use crate::error::AgentError;

const STEWARD_RUNTIME_DIR: &str = "steward-runtime";

/// Filesystem and process boundary used only by the durable steward.
///
/// The steward intentionally does not inherit the operator's normal CLI home,
/// project-local instructions, MCP configuration, plugins, memories, or skills.
/// A provider-specific auth file is linked into this private home so switching
/// to the steward does not require a second login; credential contents are never
/// copied or logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StewardIsolation {
    backend: String,
    home_dir: PathBuf,
    config_dir: PathBuf,
    workspace_dir: PathBuf,
    skills_dir: PathBuf,
    auth_linked: bool,
}

impl StewardIsolation {
    pub(crate) fn prepare(data_dir: &Path, user_id: &str, backend: &str) -> Result<Option<Self>, AgentError> {
        if !matches!(backend, "codex" | "qoder") {
            return Ok(None);
        }
        let source_home = dirs::home_dir()
            .ok_or_else(|| AgentError::internal("cannot resolve the user home needed for steward authentication"))?;
        Self::prepare_with_home(data_dir, user_id, backend, &source_home).map(Some)
    }

    fn prepare_with_home(
        data_dir: &Path,
        user_id: &str,
        backend: &str,
        source_home: &Path,
    ) -> Result<Self, AgentError> {
        let user_scope = stable_user_scope(user_id);
        let root = data_dir.join(STEWARD_RUNTIME_DIR).join(user_scope);
        let home_dir = root.join("home");
        let workspace_dir = root.join("workspace");
        let skills_dir = workspace_dir.join("skills");
        let config_dir = match backend {
            "codex" => home_dir.join(".codex"),
            "qoder" => home_dir.join(".qoder"),
            _ => return Err(AgentError::bad_request("unsupported steward isolation backend")),
        };

        for dir in [&home_dir, &workspace_dir, &skills_dir, &config_dir] {
            fs::create_dir_all(dir).map_err(|error| isolation_io_error("create directory", dir, error))?;
        }

        // macOS resolves the per-user default keychain through HOME. A private
        // steward HOME therefore has no default keychain, and provider CLIs
        // using Security.framework repeatedly open a "Keychain Not Found"
        // dialog when refreshing credentials. Write only the default-keychain
        // preference into the isolated HOME; keep all ordinary config, MCP and
        // skills isolated and leave the operator's keychain settings untouched.
        configure_macos_default_keychain(&home_dir, source_home)?;

        // Both providers see the same steward-only skill collection. The real
        // files live inside the isolated workspace so the default workspace-write
        // sandbox can author and refine them without granting access elsewhere.
        ensure_dir_link(&skills_dir, &config_dir.join("skills"))?;

        let (source_auth, isolated_auth) = match backend {
            "codex" => (source_home.join(".codex/auth.json"), config_dir.join("auth.json")),
            "qoder" => (source_home.join(".qoder/.auth"), config_dir.join(".auth")),
            _ => unreachable!("backend validated above"),
        };
        let auth_linked = if source_auth.is_file() {
            ensure_file_link(&source_auth, &isolated_auth)?;
            true
        } else if source_auth.is_dir() {
            // Qoder stores its credential set in ~/.qoder/.auth as a directory
            // (confirmed from the installed CLI). Keep the directory linked as
            // a unit so token refreshes remain visible without copying secrets.
            ensure_dir_link(&source_auth, &isolated_auth)?;
            true
        } else {
            false
        };

        Ok(Self {
            backend: backend.to_owned(),
            home_dir,
            config_dir,
            workspace_dir,
            skills_dir,
            auth_linked,
        })
    }

    pub(crate) fn workspace(&self) -> String {
        self.workspace_dir.to_string_lossy().into_owned()
    }

    pub(crate) fn auth_linked(&self) -> bool {
        self.auth_linked
    }

    pub(crate) fn append_spawn_env(&self, env: &mut Vec<(String, String)>) {
        upsert_pair(env, "HOME", self.home_dir.to_string_lossy().into_owned());
        if self.backend == "codex" {
            upsert_pair(env, "CODEX_HOME", self.config_dir.to_string_lossy().into_owned());
        }
        upsert_pair(
            env,
            "AIONUI_STEWARD_SKILLS_DIR",
            self.skills_dir.to_string_lossy().into_owned(),
        );
    }

    pub(crate) fn apply_to_acp_command(&self, command: &mut CommandSpec) {
        upsert_command_env(command, "HOME", self.home_dir.to_string_lossy().into_owned());
        upsert_command_env(
            command,
            "AIONUI_STEWARD_SKILLS_DIR",
            self.skills_dir.to_string_lossy().into_owned(),
        );
        command.cwd = Some(self.workspace());

        if self.backend == "qoder" {
            // `qodercli --help` documents --config-dir as the custom user-level
            // config root. HOME is private as well because Qoder also discovers
            // ~/.agents/skills independently of that option.
            command.args.push("--config-dir".to_owned());
            command.args.push(self.config_dir.to_string_lossy().into_owned());
        }
    }

    pub(crate) fn extend_preset_context(&self, preset: Option<String>) -> String {
        let base = preset.unwrap_or_default();
        format!(
            "{base}\n\n[Steward Isolated Runtime]\nThis steward runs in a private CLI home and workspace. Do not read or modify the user's ordinary Agent MCP, Skills, plugins, memories, or configuration. The only MCP available for task control is the AionUi steward control server. Steward-only Skills live under `{skills}`. When the user asks to create or optimize a steward Skill, edit only a dedicated subdirectory there and keep its entry point in `SKILL.md`; never write to the normal Codex or Qoder home. These Skills should improve task triage, session lookup, progress reporting, interruption recovery, or safe session control, not perform the delegated worker's domain labor.\n",
            skills = self.skills_dir.display()
        )
    }
}

fn stable_user_scope(user_id: &str) -> String {
    let digest = Sha256::digest(user_id.as_bytes());
    hex::encode(&digest[..12])
}

fn upsert_pair(env: &mut Vec<(String, String)>, name: &str, value: String) {
    env.retain(|(existing, _)| existing != name);
    env.push((name.to_owned(), value));
}

fn upsert_command_env(command: &mut CommandSpec, name: &str, value: String) {
    command.env.retain(|entry| entry.name != name);
    command.env.push(EnvVar {
        name: name.to_owned(),
        value,
    });
}

fn ensure_file_link(source: &Path, destination: &Path) -> Result<(), AgentError> {
    ensure_link(source, destination, false)
}

fn ensure_dir_link(source: &Path, destination: &Path) -> Result<(), AgentError> {
    ensure_link(source, destination, true)
}

fn ensure_link(source: &Path, destination: &Path, is_dir: bool) -> Result<(), AgentError> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if !metadata.file_type().is_symlink() {
                return Err(AgentError::conflict(format!(
                    "steward isolation path already exists and is not a link: {}",
                    destination.display()
                )));
            }
            let current =
                fs::read_link(destination).map_err(|error| isolation_io_error("read link", destination, error))?;
            if current != source {
                return Err(AgentError::conflict(format!(
                    "steward isolation link points to an unexpected target: {}",
                    destination.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_link(source, destination, is_dir),
        Err(error) => Err(isolation_io_error("inspect path", destination, error)),
    }
}

#[cfg(unix)]
fn create_link(source: &Path, destination: &Path, _is_dir: bool) -> Result<(), AgentError> {
    std::os::unix::fs::symlink(source, destination)
        .map_err(|error| isolation_io_error("create link", destination, error))
}

#[cfg(windows)]
fn create_link(source: &Path, destination: &Path, is_dir: bool) -> Result<(), AgentError> {
    let result = if is_dir {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    };
    result.map_err(|error| isolation_io_error("create link", destination, error))
}

fn isolation_io_error(action: &str, path: &Path, error: std::io::Error) -> AgentError {
    AgentError::internal(format!(
        "failed to {action} for steward isolation at {}: {error}",
        path.display()
    ))
}

#[cfg(target_os = "macos")]
fn configure_macos_default_keychain(home_dir: &Path, source_home: &Path) -> Result<(), AgentError> {
    let login_keychain = source_home.join("Library/Keychains/login.keychain-db");
    if !login_keychain.is_file() {
        return Ok(());
    }

    let preferences_dir = home_dir.join("Library/Preferences");
    fs::create_dir_all(&preferences_dir)
        .map_err(|error| isolation_io_error("create macOS keychain preferences directory", &preferences_dir, error))?;

    let output = Command::new("/usr/bin/security")
        .args(["default-keychain", "-d", "user", "-s"])
        .arg(&login_keychain)
        .env("HOME", home_dir)
        .output()
        .map_err(|error| isolation_io_error("configure macOS default keychain", home_dir, error))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(AgentError::internal(format!(
            "failed to configure the steward macOS default keychain: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn configure_macos_default_keychain(_home_dir: &Path, _source_home: &Path) -> Result<(), AgentError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_isolation_is_idempotent_and_only_links_auth_and_skills() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("operator");
        fs::create_dir_all(source_home.join(".codex")).unwrap();
        fs::write(source_home.join(".codex/auth.json"), "secret").unwrap();

        let first = StewardIsolation::prepare_with_home(temp.path(), "user/../one", "codex", &source_home).unwrap();
        let second = StewardIsolation::prepare_with_home(temp.path(), "user/../one", "codex", &source_home).unwrap();

        assert_eq!(first, second);
        assert!(first.auth_linked());
        assert!(first.workspace_dir.starts_with(temp.path().join(STEWARD_RUNTIME_DIR)));
        assert!(!first.workspace_dir.to_string_lossy().contains("user/../one"));
        assert_eq!(
            fs::read_link(first.config_dir.join("auth.json")).unwrap(),
            source_home.join(".codex/auth.json")
        );
        assert_eq!(
            fs::read_link(first.config_dir.join("skills")).unwrap(),
            first.skills_dir
        );
        assert!(!first.config_dir.join("config.toml").exists());
    }

    #[test]
    fn qoder_isolation_uses_private_home_config_and_shared_skill_root() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("operator");
        fs::create_dir_all(source_home.join(".qoder")).unwrap();
        fs::create_dir_all(source_home.join(".qoder/.auth")).unwrap();
        fs::write(source_home.join(".qoder/.auth/user"), "secret").unwrap();
        let isolation = StewardIsolation::prepare_with_home(temp.path(), "user-1", "qoder", &source_home).unwrap();
        let mut command = CommandSpec::default();

        isolation.apply_to_acp_command(&mut command);

        assert_eq!(command.cwd.as_deref(), Some(isolation.workspace().as_str()));
        assert_eq!(
            command.args,
            vec!["--config-dir", isolation.config_dir.to_string_lossy().as_ref()]
        );
        assert!(
            command
                .env
                .iter()
                .any(|entry| entry.name == "HOME" && entry.value == isolation.home_dir.to_string_lossy())
        );
        assert!(command.env.iter().all(|entry| entry.name != "CODEX_HOME"));
        assert_eq!(
            fs::read_link(isolation.config_dir.join(".auth")).unwrap(),
            source_home.join(".qoder/.auth")
        );
    }

    #[test]
    fn isolation_refuses_to_overwrite_an_existing_auth_file() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("operator");
        fs::create_dir_all(source_home.join(".qoder")).unwrap();
        fs::create_dir_all(source_home.join(".qoder/.auth")).unwrap();
        fs::write(source_home.join(".qoder/.auth/user"), "source").unwrap();

        let initial = StewardIsolation::prepare_with_home(temp.path(), "user-1", "qoder", &source_home).unwrap();
        fs::remove_file(initial.config_dir.join(".auth")).unwrap();
        fs::write(initial.config_dir.join(".auth"), "unexpected").unwrap();

        let error = StewardIsolation::prepare_with_home(temp.path(), "user-1", "qoder", &source_home).unwrap_err();
        assert!(matches!(error, AgentError::Conflict(_)));
    }

    #[test]
    fn missing_provider_auth_keeps_the_isolation_but_reports_unlinked() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("operator");
        fs::create_dir_all(&source_home).unwrap();

        let isolation = StewardIsolation::prepare_with_home(temp.path(), "user-1", "codex", &source_home).unwrap();

        assert!(!isolation.auth_linked());
        assert!(!isolation.config_dir.join("auth.json").exists());
        assert!(isolation.skills_dir.is_dir());
    }
}
