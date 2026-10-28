//! Copilot: the event log `<session-state>/<session>/events.jsonl`.
//!
//! **Tip.** Copilot replays the whole log when it resumes a session, and carries on after its
//! last event: the tip is the last line capture keeps a row of.
//!
//! **Append.** Never: atuin doesn't write Copilot's event log, so a copy behind the synced
//! session is left as it is ([`SyncError::Unsupported`]).
//!
//! **Liveness.** Any `copilot` running here in the session's directory (or below it, or where that
//! can't be told) may be writing it; so may one anywhere whose command line names the session
//! (`copilot --resume=<id>`), and while the log was modified in the last
//! [`RECENT`](super::RECENT), any `copilot` at all.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::liveness::{agent_running, changed_lately};
use super::{
    AppendOptions, AppendOutcome, Dirs, Liveness, LocalTip, Processes, ReplaceOutcome, Seen,
    SessionSync, Stamp, SyncError, blocking, blocking_liveness, modified,
};
use crate::harnesstools::copilot::Copilot;
use crate::harnesstools::copilot::session::{CopilotMessage, default_root, locate};
use crate::harnesstools::rehydrate::RehydrateMessage;
use crate::harnesstools::session::Message;

const UNWRITABLE: &str = "atuin can't write Copilot sessions";

impl SessionSync for Copilot {
    async fn local_tip(&self, id: &str) -> Result<Option<LocalTip>, SyncError> {
        let (root, id) = (default_root(), id.to_owned());
        blocking(move || match locate(&root, &id) {
            Some(path) => Ok(Some(tip(&path)?)),
            None => Ok(None),
        })
        .await
    }

    async fn is_live(&self, id: &str, cwd: Option<&Path>) -> Liveness {
        let (root, id, cwd) = (default_root(), id.to_owned(), cwd.map(Path::to_path_buf));
        blocking_liveness(move || {
            let log = locate(&root, &id);
            let seen = Seen {
                dirs: Dirs::of(cwd),
                changing: log.as_deref().is_some_and(|f| changed_lately(modified(f))),
                names: vec![id],
            };
            agent_running(&Processes::here(), "copilot", &[], &seen)
        })
        .await
    }

    async fn append(
        &self,
        _id: &str,
        _base: &LocalTip,
        _lines: &[RehydrateMessage],
        _options: &AppendOptions<'_>,
    ) -> Result<AppendOutcome, SyncError> {
        Err(SyncError::Unsupported(UNWRITABLE))
    }

    async fn replace(
        &self,
        _id: &str,
        _base: &LocalTip,
        _branch: &[RehydrateMessage],
        _options: &AppendOptions<'_>,
        _backups: &Path,
    ) -> Result<ReplaceOutcome, SyncError> {
        Err(SyncError::Unsupported(UNWRITABLE))
    }
}

/// What the event log at `path` holds, read as capture reads it.
fn tip(path: &Path) -> Result<LocalTip, SyncError> {
    let modified = modified(path);
    let bytes = std::fs::read(path)?;
    let mut known = HashSet::new();
    let (mut last, mut cwd) = (None, None::<PathBuf>);
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.trim_ascii().is_empty()) {
        let Ok(message) = crate::json::js::from_slice::<CopilotMessage>(line) else {
            continue;
        };
        if let Some(dir) = message.cwd() {
            cwd = Some(dir);
        }
        if let Some(id) = message.id() {
            let id = String::from(id);
            known.insert(id.clone());
            last = Some(id);
        }
    }
    Ok(LocalTip {
        native_path: path.to_path_buf(),
        known_source_ids: known,
        merged: HashMap::new(),
        tip_source_id: last,
        stamp: Stamp::of(&bytes),
        modified,
        cwd,
        unswitchable: Some(UNWRITABLE),
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    fn the_tip_is_the_last_event_capture_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let lines = [
            json!({"id": "a", "timestamp": "2026-10-28T12:00:00Z", "type": "session.start",
                "data": {"context": {"cwd": "/work"}}}),
            json!({"id": "b", "timestamp": "2026-10-28T12:00:01Z", "type": "user.message",
                "data": {"content": "hi"}}),
            json!({"id": "c", "timestamp": "2026-10-28T12:00:02Z", "type": "session.idle",
                "data": {}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, &text).unwrap();

        let tip = tip(&path).unwrap();

        assert_eq!(tip.tip_source_id.as_deref(), Some("b"));
        assert_eq!(tip.known_source_ids, HashSet::from(["a".to_owned(), "b".to_owned()]));
        assert_eq!(tip.cwd, Some(PathBuf::from("/work")));
        assert_eq!(tip.stamp, Stamp::of(text.as_bytes()));
        assert!(tip.unswitchable.is_some());
    }
}
