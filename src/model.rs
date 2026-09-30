//! Serializable records and small value types shared by every driver
//! and printed by the CLI. Field names are the JSON contract (§5.3).

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

/// A stable pane address: a tmux pane ID (`%12`) or a Herdr pane ID
/// (`w1:p2`). It never changes on rename.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Handle(pub String);

impl fmt::Display for Handle {
    /// Writes the raw handle string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An agent's coarse execution state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The agent is busy.
    Working,
    /// The agent waits for input.
    Idle,
    /// The agent waits on a dialog or permission prompt.
    Blocked,
    /// The state cannot be determined.
    #[value(skip)]
    Unknown,
}

/// How far a reported value can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// The harness tracks it directly.
    Native,
    /// Derived from pattern matching on pane text.
    Heuristic,
    /// No signal at all.
    None,
}

/// What an agent session reference is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRefKind {
    /// A conversation ID.
    Id,
    /// A session file path.
    Path,
}

/// A reference to an agent's conversation, used to resume it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Whether `value` is an ID or a path.
    pub kind: SessionRefKind,
    /// The ID or path.
    pub value: String,
}

/// One workspace (Herdr workspace, tmux session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceRecord {
    /// Harness workspace ID (`w1`, `$3`).
    pub workspace_id: String,
    /// Display label.
    pub label: String,
    /// Whether the user currently looks at it.
    pub focused: bool,
    /// Tags set through `workspace tag` (and, on Herdr, other tools).
    pub tags: BTreeMap<String, String>,
}

/// One pane with everything needed to rebuild it later (§5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRecord {
    /// The pane handle.
    pub handle: Handle,
    /// Workspace ID.
    pub workspace_id: String,
    /// Workspace label, when known.
    pub workspace_label: Option<String>,
    /// Tab ID (Herdr tab, tmux window).
    pub tab_id: String,
    /// Tab label, when known.
    pub tab_label: Option<String>,
    /// Pane label, when one was set.
    pub label: Option<String>,
    /// Current working directory of the pane.
    pub cwd: Option<String>,
    /// Agent kind (`claude`, `codex`, …), when known.
    pub agent: Option<String>,
    /// Agent conversation reference, when known.
    pub agent_session: Option<AgentSession>,
    /// Trust in `agent_session`.
    pub session_confidence: Confidence,
    /// Agent status.
    pub status: Status,
    /// Trust in `status`.
    pub status_confidence: Confidence,
}

/// The answer to `pane status` and `pane wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusResult {
    /// The pane asked about.
    pub handle: Handle,
    /// Its status.
    pub status: Status,
    /// Trust in `status`.
    pub confidence: Confidence,
    /// Last lines of output for non-native results, else `None`.
    pub tail: Option<String>,
}

/// The answer to `pane spawn` and `pane split`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartResult {
    /// The new pane.
    pub handle: Handle,
    /// False when the pane exists but its command did not start.
    pub started: bool,
}

/// Which side of the target pane a split opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    /// To the right.
    Right,
    /// Below.
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod pane_record {
        use super::*;

        #[test]
        fn should_serialize_nulls_when_fields_unknown() {
            let record = PaneRecord {
                handle: Handle("%1".into()),
                workspace_id: "$0".into(),
                workspace_label: None,
                tab_id: "@0".into(),
                tab_label: None,
                label: None,
                cwd: None,
                agent: None,
                agent_session: None,
                session_confidence: Confidence::None,
                status: Status::Unknown,
                status_confidence: Confidence::None,
            };

            let json = serde_json::to_string(&record).unwrap();

            assert_eq!(
                json,
                r#"{"handle":"%1","workspace_id":"$0","workspace_label":null,"tab_id":"@0","tab_label":null,"label":null,"cwd":null,"agent":null,"agent_session":null,"session_confidence":"none","status":"unknown","status_confidence":"none"}"#,
            );
        }
    }
}
