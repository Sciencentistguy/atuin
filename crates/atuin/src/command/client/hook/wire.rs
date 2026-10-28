//! Typed wire protocol for the hook events coding agents send to `atuin hook`.
//!
//! Claude Code, Codex and Copilot invoke `atuin hook <agent>` for each tool use
//! and pass the event as JSON on stdin.

use std::path::PathBuf;

use atuin_common::string::NonNulStr;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub enum WireToolName {
    /// The tool the agent requested is a Bash command.
    Bash,
    /// Unrecognized wire tool name.
    #[serde(other)]
    Other,
}

/// A hook event exactly as an agent serializes it on stdin.
#[derive(Debug, Deserialize)]
pub struct WireHookEvent {
    /// The lifecycle stage. An unrecognized value decodes to [`HookEventName::Other`].
    pub hook_event_name: HookEventName,
    /// The tool that ran; we only record `Bash`.
    pub tool_name: WireToolName,
    /// Correlates a command's start and end across two `atuin hook` invocations.
    /// Copilot sends none (see [`Self::session_id`]).
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// The agent's session. With the command, what correlates a Copilot
    /// command's start and end, as Copilot names no tool use.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The command about to run. Present on `PreToolUse`; absent (and unread)
    /// on the completion events.
    #[serde(default)]
    pub tool_input: Option<WireToolInput>,
    /// How the command finished. Present on `PostToolUse`; absent elsewhere.
    #[serde(default)]
    pub tool_response: Option<WireToolResponse>,
    /// How the command finished, on Copilot's `PostToolUse`.
    #[serde(default)]
    pub tool_result: Option<WireToolResult>,
    /// Why the command failed, on Claude Code's `PostToolUseFailure`: for a shell command that
    /// exited non-zero, `Exit code <n>` and then its output. A value, as Copilot's may not be
    /// text: an event that failed to decode would leave its command's entry open.
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    /// The session's transcript, where Codex records a command's exit code (its
    /// `tool_response` is only the output).
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
}

/// The lifecycle stage an event represents.
///
/// The wire values are `PascalCase` and match these variant names exactly.
/// Unrecognized values map to [`HookEventName::Other`] so future or
/// agent-specific events are skipped rather than rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum HookEventName {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    #[serde(other)]
    Other,
}

/// See [`WireHookEvent::tool_input`].
#[derive(Debug, Deserialize)]
pub struct WireToolInput {
    #[serde(default)]
    pub command: Option<NonNulStr>,
    #[serde(default)]
    pub description: Option<String>,
}

/// See [`WireHookEvent::tool_response`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum WireToolResponse {
    /// Some harnesses (ie. Codex) only return the tool output string.
    Output(String),
    /// Other harnesses return more details.
    /// TODO(markovejnovic): Should we split the enum per-harness?
    Object {
        #[serde(rename = "exitCode", default)]
        exit_code: Option<i64>,
    },
}

impl WireToolResponse {
    /// The exit code the agent reported, if any.
    pub fn exit_code(&self) -> Option<i64> {
        match self {
            WireToolResponse::Output(_) => None,
            WireToolResponse::Object { exit_code } => *exit_code,
        }
    }
}

/// See [`WireHookEvent::tool_result`].
#[derive(Debug, Deserialize)]
pub struct WireToolResult {
    /// `success`, `failure`, `rejected` or `denied`.
    #[serde(default)]
    pub result_type: Option<String>,
    /// What the model was given: the command's output, ending in the exit code
    /// (`<exited with exit code 1>`).
    #[serde(default)]
    pub text_result_for_llm: Option<String>,
}

impl WireToolResult {
    /// The exit code the output ends with, else 1 for a call that failed.
    pub fn exit_code(&self) -> Option<i64> {
        let said = self.text_result_for_llm.as_deref().and_then(|text| {
            let (_, code) = text.trim_end().lines().last()?.rsplit_once("exit code ")?;
            code.trim_end_matches('>').trim().parse().ok()
        });
        said.or_else(|| (self.result_type.as_deref() == Some("failure")).then_some(1))
    }
}
