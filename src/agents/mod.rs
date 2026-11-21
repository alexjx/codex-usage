//! Closed-enum adapter dispatch. Each agent owns its native identities and
//! reconciliation; shared consumers see only normalized model types.
pub mod claude_code;
pub mod claude_conversation;
pub mod codex;
pub mod codex_conversation;

use crate::{
    cancel,
    model::{
        AgentKind, Coverage, EventKey, MessageTiming, SessionInfo, SessionKey, Source, UsageEvent,
    },
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

/// A discovered transcript file plus the layout facts its path proves.
#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub hint: FileHint,
}
#[derive(Debug, Clone)]
pub enum FileHint {
    Codex,
    ClaudeMain { session_id: String },
    ClaudeSubagent { parent_id: String, agent_id: String },
}
impl DiscoveredFile {
    pub fn codex(path: PathBuf) -> Self {
        Self {
            path,
            hint: FileHint::Codex,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RecordLocation<'a> {
    pub path: &'a Path,
    pub line: u64,
}

#[derive(Clone)]
pub enum ParserState {
    Codex(Box<codex::Parser>),
    ClaudeCode(Box<claude_code::Parser>),
}
impl ParserState {
    pub fn new(agent: AgentKind, source: &str, file: &DiscoveredFile) -> Self {
        match agent {
            AgentKind::Codex => Self::Codex(Box::new(codex::Parser::new(source, &file.path))),
            AgentKind::ClaudeCode => {
                Self::ClaudeCode(Box::new(claude_code::Parser::new(source, file)))
            }
        }
    }
    pub fn agent(&self) -> AgentKind {
        match self {
            Self::Codex(_) => AgentKind::Codex,
            Self::ClaudeCode(_) => AgentKind::ClaudeCode,
        }
    }
    pub fn ingest(&mut self, bytes: &[u8], at: RecordLocation<'_>) {
        match self {
            Self::Codex(parser) => parser.ingest(bytes, at),
            Self::ClaudeCode(parser) => parser.ingest(bytes, at),
        }
    }
    pub fn malformed_final(&mut self, at: RecordLocation<'_>) {
        match self {
            Self::Codex(parser) => parser.malformed_final(at),
            Self::ClaudeCode(parser) => parser.malformed_final(at),
        }
    }
    pub fn add_pending(&mut self) {
        match self {
            Self::Codex(parser) => parser.coverage.pending += 1,
            Self::ClaudeCode(parser) => parser.coverage.pending += 1,
        }
    }
    pub fn coverage(&self) -> &Coverage {
        match self {
            Self::Codex(parser) => &parser.coverage,
            Self::ClaudeCode(parser) => &parser.coverage,
        }
    }
    pub fn quota_snapshots(&self) -> &[crate::quota::QuotaSnapshot] {
        match self {
            Self::Codex(parser) => &parser.quota_snapshots,
            Self::ClaudeCode(_) => &[],
        }
    }
    /// Compaction markers whose token usage was never recorded.
    pub fn missing_compactions(&self) -> usize {
        match self {
            Self::Codex(parser) => parser.missing_compactions(),
            Self::ClaudeCode(_) => 0,
        }
    }
    pub fn snapshot(&self) -> ParsedFileSnapshot {
        match self {
            Self::Codex(parser) => ParsedFileSnapshot::Codex(Box::new(parser.snapshot())),
            Self::ClaudeCode(parser) => ParsedFileSnapshot::ClaudeCode(Box::new(parser.snapshot())),
        }
    }
}

#[derive(Clone)]
pub enum ParsedFileSnapshot {
    Codex(Box<codex::Snapshot>),
    ClaudeCode(Box<claude_code::Snapshot>),
}

/// Normalized output of one agent's cross-file reconciliation.
#[derive(Debug)]
pub struct AgentScan {
    pub events: Vec<UsageEvent>,
    pub message_timings: Vec<MessageTiming>,
    pub observed_event_ids: BTreeSet<EventKey>,
    pub sessions: BTreeMap<SessionKey, SessionInfo>,
    pub coverage: Coverage,
    /// Diagnostics affecting a specific source. These describe source quality
    /// for quota forecasts and are not added to the global counters again.
    pub source_coverage: BTreeMap<String, Coverage>,
}

pub fn discover(source: &Source, stopped: &AtomicBool) -> Result<Vec<DiscoveredFile>> {
    match source.agent {
        AgentKind::Codex => codex::discover(&source.root, stopped),
        AgentKind::ClaudeCode => claude_code::discover(&source.root, stopped),
    }
}

pub fn reconcile(
    agent: AgentKind,
    snapshots: &[ParsedFileSnapshot],
    stopped: &AtomicBool,
) -> Result<AgentScan> {
    cancel::check(stopped)?;
    match agent {
        AgentKind::Codex => codex::reconcile(snapshots, stopped),
        AgentKind::ClaudeCode => claude_code::reconcile(snapshots, stopped),
    }
}

pub fn note(coverage: &mut Coverage, message: impl Into<String>) {
    if coverage.messages.len() < 32 {
        coverage.messages.insert(message.into());
    }
}
pub fn add_coverage(to: &mut Coverage, from: &Coverage) {
    to.files += from.files;
    to.malformed += from.malformed;
    to.pending += from.pending;
    to.duplicates += from.duplicates;
    to.conflicts += from.conflicts;
    to.uncertain += from.uncertain;
    to.excluded_usage += from.excluded_usage;
    for message in &from.messages {
        note(to, message);
    }
}
pub(crate) fn string(value: &serde_json::Value, field: &str) -> Option<String> {
    value.get(field)?.as_str().map(str::to_owned)
}
pub(crate) fn timestamp(value: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    value
        .get("timestamp")?
        .as_str()?
        .parse::<chrono::DateTime<chrono::Utc>>()
        .ok()
}
