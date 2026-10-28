//! Integrations and utilities for interacting with GitHub Copilot CLI.
use std::path::PathBuf;

use serde_json::json;

use super::rehydrate::{RehydrateError, RehydrateSession};
use super::resume::{self, CwdRequirement, ResumeError, ResumePlan, ResumeTarget};
use super::{Harness, InstallHookError, json_hooks};
use crate::dirs::home_dir;
use crate::env::var_nonempty;
use crate::path::PathBufExt as _;

pub mod session;

/// Copilot's home: `$COPILOT_HOME`, else `~/.copilot`.
pub(crate) fn home() -> PathBuf {
    var_nonempty("COPILOT_HOME").map_or_else(|| home_dir().joined(".copilot"), PathBuf::from)
}

/// Hook events atuin registers for. Copilot sends events configured under these Claude Code
/// names the payloads Claude Code would (`tool_name` `Bash`, `tool_input`, snake case), where its
/// own camel-case names (`preToolUse`) get a payload of its own.
const HOOK_EVENT_TYPES: &[&str] = &["PreToolUse", "PostToolUse", "PostToolUseFailure"];

#[derive(Debug, Clone, Copy, Default)]
pub struct Copilot;

impl Copilot {
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }
}

impl Harness for Copilot {
    fn name(&self) -> &'static str {
        "copilot"
    }

    fn alias_names(&self) -> &'static [&'static str] {
        &["copilot-cli", "github-copilot"]
    }

    fn writes_sessions(&self) -> bool {
        false
    }

    /// Install the hooks on the current active user's machine, as a file of atuin's own in
    /// Copilot's personal hooks directory (`$COPILOT_HOME/hooks`, where it loads every `.json`).
    async fn install_hooks(&self) -> Result<PathBuf, InstallHookError> {
        let hooks_dir = home().joined("hooks");
        let config_path = hooks_dir.join("atuin.json");
        let command = json_hooks::hook_command(&std::env::current_exe()?, self.name())?;
        // PowerShell evaluates a quoted path as a string rather than running it.
        let entry = json!([{
            "type": "command",
            "bash": command,
            "powershell": format!("& {command}"),
            "matcher": "Bash",
        }]);
        let hooks: serde_json::Map<_, _> =
            HOOK_EVENT_TYPES.iter().map(|event| ((*event).to_owned(), entry.clone())).collect();
        let config = serde_json::to_string_pretty(&json!({"version": 1, "hooks": hooks}))? + "\n";

        let already_installed =
            tokio::fs::read_to_string(&config_path).await.is_ok_and(|existing| existing == config);
        if already_installed {
            return Err(InstallHookError::AlreadyInstalled);
        }

        tokio::fs::create_dir_all(&hooks_dir).await?;
        tokio::fs::write(&config_path, config).await?;
        Ok(config_path)
    }

    /// `copilot --resume=<id>` (the `=` keeps an id from being taken for a prompt). Copilot finds
    /// the id from any directory, but works in the directory it is started from.
    fn resume_plan(&self, target: &ResumeTarget) -> Result<ResumePlan, ResumeError> {
        let args = [format!("--resume={}", target.id)];
        Ok(resume::plan(target, CwdRequirement::Preferred, "copilot", args))
    }

    async fn locate(&self, id: &str) -> Option<PathBuf> {
        let (root, id) = (session::default_root(), id.to_owned());
        resume::blocking(move || session::locate(&root, &id)).await
    }

    async fn rehydrate(&self, _session: &RehydrateSession) -> Result<PathBuf, RehydrateError> {
        Err(RehydrateError::Unsupported("Copilot"))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::env::mock::MockEnv;

    #[rstest]
    #[tokio::test]
    async fn installs_its_own_hooks_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let env = MockEnv::install();
        env.set("COPILOT_HOME", dir.path());

        let path = Copilot.install_hooks().await.unwrap();

        assert_eq!(path, dir.path().join("hooks").joined("atuin.json"));
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config["version"], 1);
        for event in HOOK_EVENT_TYPES {
            let hook = &config["hooks"][event][0];
            assert_eq!(hook["matcher"], "Bash");
            // Run by the current executable: the test binary here, rather than atuin.
            let bash = hook["bash"].as_str().unwrap();
            assert!(bash.ends_with(" hook copilot"), "{bash}");
            assert_eq!(hook["powershell"], format!("& {bash}"));
        }
        assert!(matches!(Copilot.install_hooks().await, Err(InstallHookError::AlreadyInstalled)));
    }

    #[rstest]
    fn resumes_by_id_preferring_the_session_directory() {
        let target = ResumeTarget::new("0199-abc").with_cwd("/work");
        let plan = Copilot.resume_plan(&target).unwrap();
        assert_eq!(plan.program, "copilot");
        assert_eq!(plan.args, ["--resume=0199-abc"]);
        assert_eq!(plan.cwd_requirement, CwdRequirement::Preferred);
    }
}
