//! GitHub Copilot CLI sessions: one directory per session under `$COPILOT_HOME/session-state`,
//! named by the session's id, holding the session's event log `events.jsonl` (Copilot's
//! `session-events.schema.json`: one `{id, timestamp, parentId, type, data}` event per line).
//!
//! Three Copilot behaviours shape this reader:
//!
//! - **The log is linear.** Each event's `parentId` is only the event written before it, so lines
//!   name no parent here: capture strings a Copilot session's rows together in time order.
//! - **Only some events are conversation.** The log also records hooks, turns, permission
//!   prompts and the like; those make no row. A subagent's events go into its parent's log, marked
//!   with its `agentId`: the parent's own tool call and result already stand for its work, so they
//!   make no row either.
//! - **Per-call usage is not kept.** Copilot never writes `assistant.usage` to the log (it is
//!   ephemeral), so a session's usage is what its assistant messages say: their output tokens.
//! - **Titles are kept beside the log.** Copilot writes no line when a session's title changes
//!   (`session.title_changed` is ephemeral too): the name the user gave it and the summary
//!   Copilot wrote of it are in the session's `workspace.yaml`, which is looked at as the log is
//!   read, each change to it going on the next line that makes a row ([`TitleFollower`]).

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;
use typed_builder::TypedBuilder;
use yaml_rust2::YamlLoader;

use crate::fs::tree_watcher::{FileStat, TreeWatcher};
use crate::harnesstools::copilot::{Copilot, home};
use crate::harnesstools::resume;
use crate::harnesstools::session::model::{
    Content, MessageId, Role, StopReason, TitleChange, TitleSource, ToolCallId, ToolResult,
    ToolUse, Usage,
};
use crate::harnesstools::session::{
    Checkpoint, Listener, Message, MessageError, Observable, RuntimeError, Session, SessionId,
    Sessions, WatchError, modified_after, scan_sessions,
};
use crate::io::{FollowLines, Line, PathLineReader, PooledReadLines};
use crate::json::jsonl::JsonlExt;
use crate::path::{AbsPathBuf, PathBufExt as _};
use crate::sync::BlockingPool;

/// The event log in each session's directory.
const EVENTS: &str = "events.jsonl";

/// What Copilot keeps of a session beside its log: where it works, and its titles.
const WORKSPACE: &str = "workspace.yaml";

#[derive(Debug, Clone, TypedBuilder)]
pub struct CopilotSessions {
    #[builder(default, setter(strip_option, into))]
    root: Option<PathBuf>,
    /// Runs every file read of the sessions this finds.
    pool: BlockingPool,
}

impl CopilotSessions {
    fn resolve_root(&self) -> PathBuf {
        self.root.clone().unwrap_or_else(default_root)
    }
}

/// Where Copilot keeps sessions: `$COPILOT_HOME/session-state`, else `~/.copilot/session-state`.
pub(crate) fn default_root() -> PathBuf {
    home().joined("session-state")
}

/// The event log of session `id` under `root`: `<id>/events.jsonl`.
pub(crate) fn locate(root: &Path, id: &str) -> Option<PathBuf> {
    if !resume::is_plain_name(id) {
        return None;
    }
    Some(root.join(id).joined(EVENTS)).filter(|path| path.is_file())
}

/// Whether `path`, under the session directory `root`, is a session's event log:
/// `<session>/events.jsonl`. Copilot keeps more in a session's directory (checkpoints, files the
/// session wrote) and ignores the flat `<session>.jsonl` logs older versions left in `root`.
fn is_event_log(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<_> = relative.components().map(|c| c.as_os_str()).collect();
    parts.len() == 2 && parts[1] == EVENTS
}

/// The session id an event log's directory carries.
fn session_id(path: &Path) -> Option<SessionId> {
    let dir = path.parent()?.file_name()?;
    Some(SessionId::from(dir.to_string_lossy().into_owned()))
}

impl Sessions for CopilotSessions {
    type Listener = CopilotListener;

    fn listener(&self) -> Result<CopilotListener, RuntimeError> {
        let root = self.resolve_root();
        if !root.is_dir() {
            return Err(RuntimeError::NotFound(root));
        }
        Ok(CopilotListener {
            root,
            pool: self.pool.clone(),
        })
    }

    fn existing(
        &self,
    ) -> Result<
        impl Stream<Item = Result<CopilotSession, RuntimeError>> + Send + 'static,
        RuntimeError,
    > {
        let root = self.resolve_root();
        if !root.is_dir() {
            return Err(RuntimeError::NotFound(root));
        }
        let pool = self.pool.clone();
        Ok(async_stream::stream! {
            let sessions_pool = pool.clone();
            let scan = pool
                .run(move || {
                    let sessions = root.clone();
                    scan_sessions(root, |path, is_file| {
                        if !is_file || !is_event_log(&sessions, path) {
                            return None;
                        }
                        let id = session_id(path)?;
                        Some(CopilotSession::open(id, path.to_path_buf(), sessions_pool.clone()))
                    })
                })
                .await;
            match scan {
                Ok(items) => {
                    for item in items {
                        yield item;
                    }
                }
                Err(cancelled) => yield Err(RuntimeError::Io(std::io::Error::other(cancelled))),
            }
        })
    }
}

impl Observable for Copilot {
    type Sessions = CopilotSessions;

    fn sessions(&self, pool: BlockingPool) -> CopilotSessions {
        CopilotSessions::builder().pool(pool).build()
    }
}

#[derive(Debug, Clone)]
pub struct CopilotListener {
    root: PathBuf,
    pool: BlockingPool,
}

impl Listener for CopilotListener {
    type Session = CopilotSession;

    fn watch(self) -> impl Stream<Item = Result<CopilotSession, WatchError>> + Send + 'static {
        let (root, pool) = (self.root, self.pool);
        async_stream::stream! {
            // The watcher reports paths under the canonical root.
            let sessions = AbsPathBuf::canonicalize(&root).map_or_else(|_| root.clone(), PathBuf::from);
            let files = TreeWatcher::builder()
                .filter(move |path| is_event_log(&sessions, path))
                .watch(&root);
            let files = match files {
                Ok(files) => files,
                Err(err) => {
                    yield Err(WatchError::from(err));
                    return;
                }
            };
            for await file in files {
                let (path, stat) = file.into_parts();
                let Some(id) = session_id(&path) else { continue };
                yield Ok(CopilotSession {
                    id,
                    path: path.to_path_buf(),
                    changes: Some(stat),
                    pool: pool.clone(),
                });
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CopilotSession {
    id: SessionId,
    path: PathBuf,
    /// Wakes [`messages`](Session::messages) on each change to the file; `None` reads it once.
    changes: Option<watch::Receiver<FileStat>>,
    pool: BlockingPool,
}

impl CopilotSession {
    /// A session over the file as it stands: [`messages`](Session::messages) ends at its end.
    #[must_use]
    pub fn open(id: SessionId, path: PathBuf, pool: BlockingPool) -> Self {
        Self {
            id,
            path,
            changes: None,
            pool,
        }
    }

    /// The byte offset to read from: `from`'s, if the line ending there is still the one `from`
    /// was taken after, else the log's start. Copilot rewrites the log shorter when the user
    /// rewinds the session to a snapshot.
    async fn start(&self, from: Checkpoint) -> u64 {
        let path = self.path.clone();
        let found = self
            .pool
            .run(move || File::open(path).and_then(|file| Line::ending_at(&file, from.at)))
            .await
            .map_err(io::Error::other)
            .and_then(|found| found);
        match found {
            Ok(Some(line)) if from.names(&line.bytes) => from.at,
            Ok(_) => {
                tracing::debug!(
                    path = %self.path.display(),
                    "the checkpoint no longer names its line; reading from the start"
                );
                0
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    path = %self.path.display(),
                    "failed to read the line a checkpoint names; reading from the start"
                );
                0
            }
        }
    }
}

impl Session for CopilotSession {
    type Message = CopilotMessage;

    fn id(&self) -> SessionId {
        self.id.clone()
    }

    fn messages_from(
        self,
        from: Option<Checkpoint>,
    ) -> impl Stream<Item = Result<(Checkpoint, CopilotMessage), MessageError>> + Send + 'static
    {
        async_stream::stream! {
            let start = match from {
                Some(from) => self.start(from).await,
                None => 0,
            };
            // A checkpoint names a line's end, never 0: read from there, it no longer held.
            let over = from.is_some() && start == 0;
            let mut titles = TitleFollower::new(&self.path, self.pool.clone());
            let lines = FollowLines::new(PooledReadLines::new(
                PathLineReader::at(&self.path, start),
                self.pool,
            ));
            let messages = match self.changes {
                Some(changes) => lines.follow(changes).left_stream(),
                None => lines.read_to_end().right_stream(),
            }
            .js_json::<CopilotMessage>();
            for await item in messages {
                yield match item {
                    Ok((line, message)) => {
                        let message = titles.stamp(message).await;
                        Ok((Checkpoint::after(&line).started_over(over), message))
                    }
                    Err(err) => Err(MessageError::from(err)),
                };
            }
        }
    }

    fn read(&self) -> impl Stream<Item = Result<CopilotMessage, MessageError>> + Send + 'static {
        let messages = FollowLines::new(PooledReadLines::new(
            PathLineReader::new(&self.path),
            self.pool.clone(),
        ))
        .read_to_end()
        .js_json::<CopilotMessage>();
        let mut titles = TitleFollower::new(&self.path, self.pool.clone());
        async_stream::stream! {
            for await item in messages {
                yield match item {
                    Ok((_, message)) => Ok(titles.stamp(message).await),
                    Err(err) => Err(MessageError::from(err)),
                };
            }
        }
    }

    async fn changed_after(mut self, since: std::time::SystemTime) -> Option<Self> {
        modified_after(self.changes.as_mut(), since).await?;
        Some(self)
    }
}

/// A session's titles, as its `workspace.yaml` gives them: the name the user gave it (`/rename`)
/// and the summary Copilot wrote of it, which Copilot's own session list shows when it has no
/// name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Titles {
    name: Option<String>,
    summary: Option<String>,
}

impl Titles {
    /// The titles in the workspace file at `path`; `None` when it can't be read or parsed (it may
    /// not be written yet, or be mid-write).
    fn read(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let docs = YamlLoader::load_from_str(&text).ok()?;
        let doc = docs.first()?;
        let title = |key: &str| {
            doc[key].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned)
        };
        Some(Self {
            name: title("name"),
            summary: title("summary"),
        })
    }

    /// The title changes that take `self` to `now`, the user's name first.
    fn changes(&self, now: &Self) -> Vec<TitleChange> {
        [
            (TitleSource::Named, &self.name, &now.name),
            (TitleSource::Generated, &self.summary, &now.summary),
        ]
        .into_iter()
        .filter(|(_, was, is)| was != is)
        .map(|(source, _, is)| TitleChange::new(source, is.as_deref().unwrap_or_default()))
        .collect()
    }
}

/// Follows a session's titles in its `workspace.yaml` while its log is read, putting each change
/// on the next line that makes a row: a line can carry one title change, so a name and a summary
/// that changed together go on two. The file is read again only once it has been modified.
///
/// A title that changes while the session writes nothing more is taken up with its next line.
struct TitleFollower {
    path: PathBuf,
    pool: BlockingPool,
    /// When the titles were last read from the file, as it was then modified.
    modified: Option<SystemTime>,
    titles: Titles,
    pending: VecDeque<TitleChange>,
}

impl TitleFollower {
    /// For the session whose log is `log`.
    fn new(log: &Path, pool: BlockingPool) -> Self {
        Self {
            path: log.with_file_name(WORKSPACE),
            pool,
            modified: None,
            titles: Titles::default(),
            pending: VecDeque::new(),
        }
    }

    /// `message`, with the next title change on it when it makes a row.
    async fn stamp(&mut self, mut message: CopilotMessage) -> CopilotMessage {
        if message.id().is_none() || message.kind == "session.title_changed" {
            return message;
        }
        let (path, seen) = (self.path.clone(), self.modified);
        let read = self.pool.run(move || {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let changed = modified.is_some() && modified != seen;
            (modified, changed.then(|| Titles::read(&path)).flatten())
        });
        if let Ok((modified, Some(titles))) = read.await {
            self.pending.extend(self.titles.changes(&titles));
            self.titles = titles;
            self.modified = modified;
        }
        message.workspace_title = self.pending.pop_front();
        message
    }
}

/// One event of a Copilot session's log.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotMessage {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    timestamp: Option<String>,
    /// The subagent the event comes from; absent for the session's own events.
    agent_id: Option<String>,
    #[serde(default)]
    data: Value,
    /// A change to the session's titles in its `workspace.yaml` (see [`TitleFollower`]).
    #[serde(skip)]
    workspace_title: Option<TitleChange>,
}

/// Event types that make a row.
const CONVERSATION: &[&str] = &[
    "session.start",
    "session.resume",
    "session.context_changed",
    "session.model_change",
    "session.title_changed",
    "session.compaction_complete",
    "session.error",
    "user.message",
    "assistant.message",
    "tool.execution_complete",
    "abort",
];

impl CopilotMessage {
    /// The event's data, for an event of the session's own conversation (see the module docs).
    fn data(&self) -> Option<&Value> {
        (self.agent_id.is_none() && CONVERSATION.contains(&self.kind.as_str()))
            .then_some(&self.data)
    }

    fn data_str(&self, key: &str) -> Option<&str> {
        self.data()?.get(key)?.as_str()
    }

    /// The working directory context a session start, resume or directory change records.
    fn context(&self) -> Option<&Value> {
        match self.kind.as_str() {
            "session.start" | "session.resume" => self.data()?.get("context"),
            "session.context_changed" => self.data(),
            _ => None,
        }
    }

    /// What a user message attaches, as what it was: not a file's or a selection's contents.
    fn attachment(value: &Value) -> Content {
        let kind = value["type"].as_str().unwrap_or_default();
        let named = ["path", "displayName", "url", "uri"]
            .into_iter()
            .find_map(|key| value.get(key).filter(|v| v.is_string()).map(|v| (key, v.clone())));
        let mut attachment = serde_json::json!({"type": kind});
        if let Some((key, name)) = named {
            attachment[key] = name;
        }
        Content::Other(attachment)
    }

    fn assistant_content(data: &Value) -> Vec<Content> {
        let mut content = Vec::new();
        match data["reasoningText"].as_str().filter(|t| !t.is_empty()) {
            Some(text) => content.push(Content::Reasoning(text.to_owned())),
            None if data.get("reasoningOpaque").is_some()
                || data.get("encryptedContent").is_some() =>
            {
                content.push(Content::ReasoningSummary { tokens: None });
            }
            None => {}
        }
        if let Some(text) = data["content"].as_str().filter(|t| !t.is_empty()) {
            content.push(Content::Text(text.to_owned()));
        }
        content.extend(data["toolRequests"].as_array().into_iter().flatten().map(|request| {
            Content::ToolUse(ToolUse {
                id: ToolCallId::from(request["toolCallId"].as_str().unwrap_or_default().to_owned()),
                name: request["name"].as_str().unwrap_or_default().to_owned(),
                input: request["arguments"].clone(),
            })
        }));
        content
    }

    /// A tool call's result: what the model was given, or why the call failed.
    fn tool_result(data: &Value) -> Content {
        let success = data["success"].as_bool().unwrap_or(false);
        let output = if success {
            data["result"]["content"].clone()
        } else {
            data["error"]["message"].clone()
        };
        let output = match output {
            Value::Null if !success => data["result"]["content"].clone(),
            output => output,
        };
        Content::ToolResult(ToolResult {
            call: ToolCallId::from(data["toolCallId"].as_str().unwrap_or_default().to_owned()),
            output,
            error: !success,
        })
    }
}

impl Message for CopilotMessage {
    /// The event's own id, for an event that makes a row. Events are never rewritten in place,
    /// so a line keeps its id when the log is read again.
    fn id(&self) -> Option<MessageId> {
        self.data()?;
        self.id.clone().map(MessageId::from)
    }

    /// A user message Copilot sent in the user's place (a skill's, a system notice, autopilot
    /// carrying on) is sent to the model as the user's, but the user didn't write it: its role is
    /// where it came from.
    fn role(&self) -> Role {
        match self.kind.as_str() {
            "user.message" if self.data["isAutopilotContinuation"] == true => {
                Role::Other("autopilot".to_owned())
            }
            "user.message" => match self.data["source"].as_str() {
                None | Some("user") => Role::User,
                Some(source) => Role::Other(source.to_owned()),
            },
            "assistant.message" | "session.error" | "abort" => Role::Assistant,
            "tool.execution_complete" => Role::Tool,
            kind if kind.starts_with("session.") => Role::System,
            kind => Role::Other(kind.to_owned()),
        }
    }

    fn timestamp(&self) -> Option<OffsetDateTime> {
        self.timestamp.as_deref().and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
    }

    fn content(&self) -> Vec<Content> {
        let Some(data) = self.data() else {
            return Vec::new();
        };
        match self.kind.as_str() {
            "user.message" => {
                let text = data["content"].as_str().unwrap_or_default().to_owned();
                let attachments = data["attachments"].as_array().into_iter().flatten();
                std::iter::once(Content::Text(text))
                    .chain(attachments.map(Self::attachment))
                    .collect()
            }
            "assistant.message" => Self::assistant_content(data),
            "tool.execution_complete" => vec![Self::tool_result(data)],
            "session.compaction_complete" => data["summaryContent"]
                .as_str()
                .filter(|s| data["success"] == true && !s.is_empty())
                .map(|s| vec![Content::Summary(s.to_owned())])
                .unwrap_or_default(),
            "session.error" => {
                vec![Content::Error(data["message"].as_str().unwrap_or_default().to_owned())]
            }
            _ => Vec::new(),
        }
    }

    fn model(&self) -> Option<String> {
        let key = match self.kind.as_str() {
            "assistant.message" => "model",
            "session.start" | "session.resume" => "selectedModel",
            "session.model_change" => "newModel",
            _ => return None,
        };
        self.data_str(key).map(str::to_owned)
    }

    /// Only an assistant message's output tokens: Copilot keeps nothing else of a call's usage.
    fn usage(&self) -> Option<Usage> {
        if self.kind != "assistant.message" {
            return None;
        }
        let output = self.data()?.get("outputTokens")?.as_u64()?;
        Some(Usage {
            input: None,
            output: Some(output),
            cache_read: None,
            cache_write: None,
            reasoning: None,
        })
    }

    fn stop_reason(&self) -> Option<StopReason> {
        (self.kind == "abort" && self.data().is_some()).then_some(StopReason::Aborted)
    }

    fn cwd(&self) -> Option<PathBuf> {
        self.context()?.get("cwd")?.as_str().map(PathBuf::from)
    }

    fn git_branch(&self) -> Option<String> {
        self.context()?.get("branch")?.as_str().map(str::to_owned)
    }

    /// The API call an assistant message came from: Copilot splits one call's reply into several
    /// messages sharing its `apiCallId`.
    fn turn_id(&self) -> Option<String> {
        if self.kind != "assistant.message" {
            return None;
        }
        self.data_str("apiCallId")
            .or_else(|| self.data_str("messageId"))
            .or(self.id.as_deref())
            .map(|id| format!("call:{id}"))
    }

    fn title(&self) -> Option<TitleChange> {
        if self.kind != "session.title_changed" {
            return self.workspace_title.clone();
        }
        Some(TitleChange::new(TitleSource::Generated, self.data_str("title")?))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use futures::TryStreamExt;
    use rstest::{fixture, rstest};
    use serde_json::json;

    use super::*;

    fn pool() -> BlockingPool {
        BlockingPool::new(NonZeroUsize::MIN)
    }

    fn event(kind: &str, data: Value) -> CopilotMessage {
        let mut event = json!({
            "id": "e1",
            "timestamp": "2026-10-28T12:00:00.000Z",
            "parentId": null,
            "type": kind,
        });
        event["data"] = data;
        serde_json::from_value(event).unwrap()
    }

    /// A session-state directory holding one session, `s1`, and what Copilot keeps beside it.
    #[fixture]
    fn session_state() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s1");
        std::fs::create_dir_all(session.join("checkpoints")).unwrap();
        let lines = [
            json!({"id": "a", "timestamp": "2026-10-28T12:00:00.000Z", "parentId": null,
                "type": "session.start", "data": {"sessionId": "s1", "version": 1,
                "producer": "copilot-agent", "copilotVersion": "1.0.63",
                "startTime": "2026-10-28T12:00:00.000Z", "selectedModel": "claude-sonnet-4.5",
                "context": {"cwd": "/work", "branch": "main"}}}),
            json!({"id": "b", "timestamp": "2026-10-28T12:00:01.000Z", "parentId": "a",
                "type": "user.message", "data": {"content": "list the files"}}),
            json!({"id": "c", "timestamp": "2026-10-28T12:00:02.000Z", "parentId": "b",
                "type": "assistant.turn_start", "data": {"turnId": "0"}}),
            json!({"id": "d", "timestamp": "2026-10-28T12:00:03.000Z", "parentId": "c",
                "type": "assistant.message", "data": {"messageId": "m1", "content": "",
                "toolRequests": [{"toolCallId": "t1", "name": "bash",
                "arguments": {"command": "ls"}}]}}),
            json!({"id": "e", "timestamp": "2026-10-28T12:00:04.000Z", "parentId": "d",
                "type": "tool.execution_complete", "data": {"toolCallId": "t1",
                "success": true, "result": {"content": "Cargo.toml\n"}}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(session.join(EVENTS), text).unwrap();
        std::fs::write(dir.path().join("legacy.jsonl"), "{}\n").unwrap();
        std::fs::write(session.join("checkpoints").joined("index.md"), "").unwrap();
        dir
    }

    #[rstest]
    #[tokio::test]
    async fn existing_finds_each_sessions_event_log(session_state: tempfile::TempDir) {
        let sessions = CopilotSessions::builder().root(session_state.path()).pool(pool()).build();
        let found: Vec<_> = sessions.existing().unwrap().try_collect().await.unwrap();
        assert_eq!(found.len(), 1);
        let session = found.into_iter().next().unwrap();
        assert_eq!(session.id(), SessionId::from("s1".to_owned()));

        let messages: Vec<_> = session.messages().try_collect().await.unwrap();
        let kept: Vec<_> = messages.iter().filter_map(Message::id).map(String::from).collect();
        // The turn start is bookkeeping.
        assert_eq!(kept, ["a", "b", "d", "e"]);
        assert_eq!(messages[0].cwd(), Some(PathBuf::from("/work")));
        assert_eq!(messages[0].git_branch().as_deref(), Some("main"));
        assert_eq!(messages[0].model().as_deref(), Some("claude-sonnet-4.5"));
    }

    #[rstest]
    fn locates_a_session_by_its_directory(session_state: tempfile::TempDir) {
        let root = session_state.path();
        assert_eq!(locate(root, "s1"), Some(root.join("s1").joined(EVENTS)));
        assert_eq!(locate(root, "missing"), None);
        assert_eq!(locate(root, "../s1"), None);
    }

    #[rstest]
    #[case::event_log("s1/events.jsonl", true)]
    #[case::legacy_flat_log("s1.jsonl", false)]
    #[case::nested("s1/sub/events.jsonl", false)]
    #[case::other_file("s1/plan.md", false)]
    fn tells_event_logs_apart(#[case] relative: &str, #[case] expected: bool) {
        let root = Path::new("/state");
        assert_eq!(is_event_log(root, &root.join(relative)), expected);
    }

    #[rstest]
    fn a_user_message_keeps_its_text_and_what_it_attaches() {
        let m = event("user.message", json!({
            "content": "fix @src/main.rs",
            "attachments": [{"type": "file", "path": "/w/src/main.rs", "displayName": "main.rs"}],
        }));
        assert_eq!(m.role(), Role::User);
        assert_eq!(m.content(), vec![
            Content::Text("fix @src/main.rs".into()),
            Content::Other(json!({"type": "file", "path": "/w/src/main.rs"})),
        ]);
    }

    #[rstest]
    #[case::typed(json!({"content": "hi"}), Role::User)]
    #[case::typed_and_said_so(json!({"content": "hi", "source": "user"}), Role::User)]
    #[case::from_a_skill(
        json!({"content": "use pdf", "source": "skill-pdf"}),
        Role::Other("skill-pdf".into())
    )]
    #[case::autopilot(
        json!({"content": "continue", "isAutopilotContinuation": true}),
        Role::Other("autopilot".into())
    )]
    fn a_user_message_is_the_users_only_when_typed(#[case] data: Value, #[case] role: Role) {
        assert_eq!(event("user.message", data).role(), role);
    }

    #[rstest]
    fn an_assistant_message_holds_reasoning_text_and_calls() {
        let m = event("assistant.message", json!({
            "messageId": "m1",
            "model": "gpt-5",
            "content": "Running it.",
            "reasoningText": "The user wants ls.",
            "toolRequests": [{"toolCallId": "t1", "name": "bash", "arguments": {"command": "ls"}}],
            "outputTokens": 42,
            "apiCallId": "call-7",
        }));
        assert_eq!(m.role(), Role::Assistant);
        assert_eq!(m.content(), vec![
            Content::Reasoning("The user wants ls.".into()),
            Content::Text("Running it.".into()),
            Content::ToolUse(ToolUse {
                id: ToolCallId::from("t1".to_owned()),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            }),
        ]);
        assert_eq!(m.model().as_deref(), Some("gpt-5"));
        assert_eq!(m.usage().and_then(|u| u.output), Some(42));
        assert_eq!(m.turn_id().as_deref(), Some("call:call-7"));
    }

    #[rstest]
    fn encrypted_reasoning_is_only_marked() {
        let m = event("assistant.message", json!({
            "messageId": "m1", "content": "done", "encryptedContent": "opaque",
        }));
        assert_eq!(m.content(), vec![
            Content::ReasoningSummary { tokens: None },
            Content::Text("done".into())
        ]);
    }

    #[rstest]
    #[case::success(
        json!({"toolCallId": "t1", "success": true, "result": {"content": "ok"}}),
        json!("ok"),
        false
    )]
    #[case::failure(
        json!({"toolCallId": "t1", "success": false, "error": {"message": "denied"}}),
        json!("denied"),
        true
    )]
    #[case::failure_with_output(
        json!({"toolCallId": "t1", "success": false, "result": {"content": "exit 2"}}),
        json!("exit 2"),
        true
    )]
    fn a_tool_result_says_what_the_model_got(
        #[case] data: Value,
        #[case] output: Value,
        #[case] error: bool,
    ) {
        let m = event("tool.execution_complete", data);
        assert_eq!(m.role(), Role::Tool);
        assert_eq!(m.content(), vec![Content::ToolResult(ToolResult {
            call: ToolCallId::from("t1".to_owned()),
            output,
            error,
        })]);
    }

    #[rstest]
    fn a_compaction_keeps_its_summary() {
        let m = event("session.compaction_complete", json!({
            "success": true, "summaryContent": "We fixed the build.",
        }));
        assert_eq!(m.role(), Role::System);
        assert_eq!(m.content(), vec![Content::Summary("We fixed the build.".into())]);
    }

    #[rstest]
    fn a_subagents_events_make_no_row() {
        let mut m = event("assistant.message", json!({"messageId": "m1", "content": "hi"}));
        m.agent_id = Some("agent-1".into());
        assert_eq!(m.id(), None);
        assert!(m.content().is_empty());
        assert_eq!(m.usage(), None);
    }

    #[rstest]
    #[case::hook("hook.start")]
    #[case::turn("assistant.turn_start")]
    #[case::shutdown("session.shutdown")]
    fn bookkeeping_makes_no_row(#[case] kind: &str) {
        let m = event(kind, json!({}));
        assert_eq!(m.id(), None);
        assert!(m.content().is_empty());
    }

    #[rstest]
    fn a_directory_change_moves_the_session() {
        let m = event("session.context_changed", json!({"cwd": "/elsewhere", "branch": "dev"}));
        assert_eq!(m.cwd(), Some(PathBuf::from("/elsewhere")));
        assert_eq!(m.git_branch().as_deref(), Some("dev"));
    }

    #[rstest]
    fn an_abort_stops_the_turn() {
        let m = event("abort", json!({"reason": "user initiated"}));
        assert_eq!(m.stop_reason(), Some(StopReason::Aborted));
    }

    #[rstest]
    fn a_title_change_titles_the_session() {
        let m = event("session.title_changed", json!({"title": "Fix the build"}));
        assert_eq!(m.title(), Some(TitleChange::new(TitleSource::Generated, "Fix the build")));
    }

    /// A `workspace.yaml` as Copilot writes one (`yaml.stringify`, `lineWidth: 0`).
    fn workspace(name: Option<&str>, summary: &str) -> String {
        let name = name.map(|n| format!("name: {n}\n")).unwrap_or_default();
        format!(
            "id: s1\ncwd: /work\nrepository: acme/proj\nbranch: main\n{name}summary: \"{summary}\"\n\
             summary_count: 1\ncreated_at: 2026-10-28T12:00:00.000Z\n\
             updated_at: 2026-10-28T12:01:00.000Z\n"
        )
    }

    #[rstest]
    #[case::named(
        Some("Flaky test"),
        "Fix a: flaky test",
        Some("Flaky test"),
        Some("Fix a: flaky test")
    )]
    #[case::unnamed(None, "Fix the build", None, Some("Fix the build"))]
    #[case::blank_summary(None, "  ", None, None)]
    fn reads_the_titles_from_the_workspace_file(
        #[case] name: Option<&str>,
        #[case] summary: &str,
        #[case] want_name: Option<&str>,
        #[case] want_summary: Option<&str>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WORKSPACE);
        std::fs::write(&path, workspace(name, summary)).unwrap();
        assert_eq!(Titles::read(&path), Some(Titles {
            name: want_name.map(str::to_owned),
            summary: want_summary.map(str::to_owned),
        }));
    }

    #[rstest]
    #[case::missing(None)]
    #[case::half_written(Some("name: [unclosed"))]
    fn an_unreadable_workspace_file_has_no_titles(#[case] text: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WORKSPACE);
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        assert_eq!(Titles::read(&path), None);
    }

    #[rstest]
    fn title_changes_put_the_name_first_and_clear_what_went() {
        let titles = |name: Option<&str>, summary: Option<&str>| Titles {
            name: name.map(str::to_owned),
            summary: summary.map(str::to_owned),
        };
        let none = Titles::default();
        assert_eq!(none.changes(&titles(Some("n"), Some("s"))), [
            TitleChange::new(TitleSource::Named, "n"),
            TitleChange::new(TitleSource::Generated, "s"),
        ]);
        assert_eq!(titles(Some("n"), Some("s")).changes(&titles(None, Some("s"))), [
            TitleChange::new(TitleSource::Named, "")
        ]);
        assert!(titles(None, Some("s")).changes(&titles(None, Some("s"))).is_empty());
    }

    /// The titles go on the lines that make rows, one change each, and a rename while the log is
    /// followed goes on the next.
    #[rstest]
    #[tokio::test]
    async fn the_workspace_titles_go_on_the_rows() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join(EVENTS);
        let path = dir.path().join(WORKSPACE);
        std::fs::write(&path, workspace(Some("Flaky test"), "Fix the flaky test")).unwrap();
        let line = |id: &str, kind: &str, data: Value| {
            let mut m: CopilotMessage = serde_json::from_value(json!({
                "id": id, "timestamp": "2026-10-28T12:00:00Z", "type": kind,
            }))
            .unwrap();
            m.data = data;
            m
        };
        let mut titles = TitleFollower::new(&log, pool());

        let hook = titles.stamp(line("h", "hook.start", json!({}))).await;
        let start = titles.stamp(line("a", "session.start", json!({}))).await;
        let prompt = titles.stamp(line("b", "user.message", json!({"content": "hi"}))).await;
        let reply = titles.stamp(line("c", "assistant.message", json!({"content": "ok"}))).await;
        assert_eq!(hook.title(), None, "bookkeeping makes no row to carry it");
        assert_eq!(start.title(), Some(TitleChange::new(TitleSource::Named, "Flaky test")));
        assert_eq!(
            prompt.title(),
            Some(TitleChange::new(TitleSource::Generated, "Fix the flaky test"))
        );
        assert_eq!(reply.title(), None);

        std::fs::write(&path, workspace(Some("Timing"), "Fix the flaky test")).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        let next = titles.stamp(line("d", "user.message", json!({"content": "more"}))).await;
        assert_eq!(next.title(), Some(TitleChange::new(TitleSource::Named, "Timing")));
    }

    /// A session read whole (an import) has its titles from the start.
    #[rstest]
    #[tokio::test]
    async fn an_imported_session_has_its_titles(session_state: tempfile::TempDir) {
        let session = session_state.path().join("s1");
        std::fs::write(session.join(WORKSPACE), workspace(None, "List the files")).unwrap();
        let read =
            CopilotSession::open(SessionId::from("s1".to_owned()), session.join(EVENTS), pool());
        let titles: Vec<_> = read
            .read()
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .iter()
            .filter_map(Message::title)
            .collect();
        assert_eq!(titles, [TitleChange::new(TitleSource::Generated, "List the files")]);
    }
}
