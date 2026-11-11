//! Claude Code transcript discovery and usage reconciliation.
//!
//! Recognized layouts:
//!   projects/<project>/<session>.jsonl
//!   projects/<project>/<session>/subagents/agent-<execution>.jsonl
//!
//! Usage identity is `message:<message.id>`. Fragment UUIDs identify conversation
//! blocks, not charges. `sessionId` is authoritative for main transcripts;
//! `session_id` is deliberately not treated as an alias.
use super::{note, AgentScan, DiscoveredFile, FileHint, ParsedFileSnapshot, RecordLocation};
use crate::{
    cancel,
    model::{
        AgentKind, CacheWriteBreakdown, Coverage, EventKey, SessionFile, SessionInfo, SessionKey,
        Usage, UsageEvent, UsageMode,
    },
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use walkdir::WalkDir;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
    message: Option<Message>,
}
#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<UsageRecord>,
}
#[derive(Deserialize)]
struct UsageRecord {
    input_tokens: Option<Value>,
    output_tokens: Option<Value>,
    cache_read_input_tokens: Option<Value>,
    cache_creation_input_tokens: Option<Value>,
    cache_creation: Option<Value>,
    #[allow(dead_code)]
    service_tier: Option<Value>,
    speed: Option<Value>,
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub(crate) id: Option<EventKey>,
    /// `None` means the record had an identity but unusable accounting.
    pub(crate) event: Option<UsageEvent>,
    pub(crate) file: PathBuf,
    pub(crate) session: SessionKey,
    owner_conflict: bool,
}

#[derive(Clone)]
pub(crate) struct Parser {
    source: String,
    path: PathBuf,
    file_session: String,
    execution: Option<String>,
    pub(crate) coverage: Coverage,
    candidates: Vec<Candidate>,
    sessions: BTreeMap<SessionKey, SessionInfo>,
    inferred_owner_reported: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) sessions: BTreeMap<SessionKey, SessionInfo>,
}

pub(crate) fn discover(root: &Path, stopped: &AtomicBool) -> Result<Vec<DiscoveredFile>> {
    cancel::check(stopped)?;
    fs::read_dir(root).with_context(|| format!("Cannot read source {}", root.display()))?;
    let projects = root.join("projects");
    if !projects.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(&projects).follow_links(false) {
        cancel::check(stopped)?;
        let entry = entry.with_context(|| format!("Cannot enumerate {}", projects.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "history.jsonl" || !name.ends_with(".jsonl") {
            continue;
        }
        let stem = name.trim_end_matches(".jsonl");
        let parent = path.parent();
        let hint = if parent
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("subagents")
        {
            let Some(session_dir) = parent.and_then(Path::parent) else {
                continue;
            };
            let Some(parent_id) = session_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let agent_id = stem.strip_prefix("agent-").unwrap_or(stem);
            if parent_id.is_empty() || agent_id.is_empty() {
                continue;
            }
            FileHint::ClaudeSubagent {
                parent_id: parent_id.to_owned(),
                agent_id: agent_id.to_owned(),
            }
        } else {
            if stem.is_empty() {
                continue;
            }
            FileHint::ClaudeMain {
                session_id: stem.to_owned(),
            }
        };
        files.push(DiscoveredFile {
            path: path.to_path_buf(),
            hint,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn as_u64(value: Option<&Value>) -> Option<u64> {
    value?.as_u64()
}
fn as_string(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::to_owned)
}

/// Inclusive input: raw input plus cache reads and writes exactly once.
fn normalize(usage: &UsageRecord) -> Option<(Usage, CacheWriteBreakdown, bool)> {
    let input = as_u64(usage.input_tokens.as_ref())?;
    let output = as_u64(usage.output_tokens.as_ref())?;
    let read = as_u64(usage.cache_read_input_tokens.as_ref())?;
    let create = as_u64(usage.cache_creation_input_tokens.as_ref())?;
    let total = input.checked_add(read)?.checked_add(create)?;
    let (breakdown, inconsistent) = if create == 0 {
        (CacheWriteBreakdown::default(), false)
    } else {
        match usage.cache_creation.as_ref() {
            Some(value) => {
                let five = as_u64(value.get("ephemeral_5m_input_tokens"));
                let hour = as_u64(value.get("ephemeral_1h_input_tokens"));
                match (five, hour) {
                    (Some(five), Some(hour)) => {
                        let sum = five.checked_add(hour);
                        if sum == Some(create) {
                            let mut breakdown = CacheWriteBreakdown::default();
                            if five > 0 {
                                breakdown.by_ttl_seconds.insert(300, five);
                            }
                            if hour > 0 {
                                breakdown.by_ttl_seconds.insert(3600, hour);
                            }
                            (breakdown, false)
                        } else {
                            (CacheWriteBreakdown::unknown(create), true)
                        }
                    }
                    _ => (CacheWriteBreakdown::unknown(create), true),
                }
            }
            // No duration recorded: tokens stay usable, writes stay unpriced
            // unless an explicit unknown-duration rate exists.
            None => (CacheWriteBreakdown::unknown(create), false),
        }
    };
    let usage = Usage {
        input: total,
        cached: Some(read),
        cache_write: Some(create),
        output,
        reasoning: None,
    };
    usage.validate().ok()?;
    Some((usage, breakdown, inconsistent))
}

fn mode(usage: &UsageRecord) -> UsageMode {
    // Recorded speed is the primary evidence; a known standard tier is the only
    // service_tier value that maps by itself. Unknown speed never becomes "fast".
    match as_string(usage.speed.as_ref()).as_deref() {
        Some("standard") => UsageMode::Regular,
        Some("fast") => UsageMode::Fast,
        _ => match as_string(usage.service_tier.as_ref()).as_deref() {
            Some("standard" | "standard_only") => UsageMode::Regular,
            _ => UsageMode::Unknown,
        },
    }
}

impl Parser {
    pub(crate) fn new(source: &str, file: &DiscoveredFile) -> Self {
        let (file_session, execution, layout) = match &file.hint {
            FileHint::ClaudeMain { session_id } => (session_id.clone(), None, "claude-main"),
            FileHint::ClaudeSubagent {
                parent_id,
                agent_id,
            } => (parent_id.clone(), Some(agent_id.clone()), "claude-subagent"),
            FileHint::Codex => (String::new(), None, "unrecognized"),
        };
        let (key, parent) = match &execution {
            Some(execution) => (
                SessionKey::execution(AgentKind::ClaudeCode, file_session.clone(), execution),
                Some(SessionKey::main(
                    AgentKind::ClaudeCode,
                    file_session.clone(),
                )),
            ),
            None => (
                SessionKey::main(AgentKind::ClaudeCode, file_session.clone()),
                None,
            ),
        };
        let mut sessions = BTreeMap::new();
        let mut info = SessionInfo::new(key.clone());
        info.parent = parent.clone();
        info.origins.insert(source.into());
        info.files.push(SessionFile {
            source: source.into(),
            path: file.path.clone(),
            layout: layout.into(),
        });
        sessions.insert(key, info);
        if let Some(parent) = parent {
            let mut info = SessionInfo::new(parent.clone());
            info.origins.insert(source.into());
            if !file_session.is_empty() {
                info.files.push(SessionFile {
                    source: source.into(),
                    path: file.path.clone(),
                    layout: "claude-subagent".into(),
                });
            }
            sessions.entry(parent).or_insert(info);
        }
        Self {
            source: source.into(),
            path: file.path.clone(),
            file_session,
            execution,
            coverage: Coverage::default(),
            candidates: Vec::new(),
            sessions,
            inferred_owner_reported: false,
        }
    }

    fn session(&self, owner_conflict: bool) -> SessionKey {
        match &self.execution {
            Some(execution) if !owner_conflict => SessionKey::execution(
                AgentKind::ClaudeCode,
                self.file_session.clone(),
                execution.clone(),
            ),
            Some(_) => SessionKey::main(AgentKind::ClaudeCode, self.file_session.clone()),
            None => SessionKey::main(AgentKind::ClaudeCode, self.file_session.clone()),
        }
    }

    pub(crate) fn malformed_final(&mut self, at: RecordLocation<'_>) {
        self.problem(at, "Malformed final JSON record", true);
    }

    fn problem(&mut self, at: RecordLocation<'_>, message: &str, malformed: bool) {
        if malformed {
            self.coverage.malformed += 1;
        } else {
            self.coverage.uncertain += 1;
        }
        note(
            &mut self.coverage,
            format!("{}:{}: {message}", at.path.display(), at.line),
        );
    }

    pub(crate) fn ingest(&mut self, bytes: &[u8], at: RecordLocation<'_>) {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let record: Record = match serde_json::from_slice(bytes) {
            Ok(record) => record,
            Err(error) => {
                self.problem(
                    at,
                    &format!("Malformed Claude transcript record: {error}"),
                    true,
                );
                return;
            }
        };
        if record.kind.as_deref() != Some("assistant") {
            return;
        }
        let Some(message) = record.message.as_ref() else {
            self.problem(at, "Assistant record has no message object", false);
            return;
        };
        let Some(id) = message.id.as_deref().filter(|id| !id.is_empty()) else {
            // No strong identity: usage cannot be attributed. Text remains viewable.
            self.coverage.excluded_usage += 1;
            note(
                &mut self.coverage,
                format!(
                    "{}:{}: assistant usage record has no message.id; excluded",
                    at.path.display(),
                    at.line
                ),
            );
            return;
        };
        let key = EventKey::new(AgentKind::ClaudeCode, format!("message:{id}"));

        let mut owner_conflict = false;
        if self.execution.is_none() {
            match record.session_id.as_deref() {
                Some(session) if session != self.file_session => {
                    owner_conflict = true;
                    self.problem(
                        at,
                        "sessionId contradicts the recognized transcript filename; ownership conflict",
                        false,
                    );
                }
                Some(_) => {}
                None => self.infer_owner(at),
            }
        } else if let (Some(recorded), Some(expected)) =
            (record.agent_id.as_deref(), &self.execution)
        {
            if recorded != expected {
                owner_conflict = true;
                self.problem(
                    at,
                    "agentId contradicts agent-<execution>.jsonl; ownership conflict",
                    false,
                );
            }
        } else if record.agent_id.is_none() {
            self.infer_owner(at);
        }

        let session = self.session(owner_conflict);
        let Some(usage_record) = message.usage.as_ref() else {
            self.candidates.push(Candidate {
                id: Some(key),
                event: None,
                file: self.path.clone(),
                session,
                owner_conflict,
            });
            self.problem(at, "Assistant usage lacks message.usage", false);
            return;
        };
        let normalized = normalize(usage_record);
        let Some((usage, breakdown, inconsistent)) = normalized else {
            self.candidates.push(Candidate {
                id: Some(key),
                event: None,
                file: self.path.clone(),
                session,
                owner_conflict,
            });
            self.problem(
                at,
                "Assistant usage lacks a required nonnegative token count",
                false,
            );
            return;
        };
        if inconsistent {
            self.problem(
                at,
                "cache_creation TTL breakdown is inconsistent; tokens retained, write pricing unavailable",
                false,
            );
        }
        let Some(timestamp) = record
            .timestamp
            .as_deref()
            .and_then(|text| text.parse::<DateTime<Utc>>().ok())
        else {
            self.candidates.push(Candidate {
                id: Some(key),
                event: None,
                file: self.path.clone(),
                session,
                owner_conflict,
            });
            self.problem(at, "Assistant usage record has no valid timestamp", false);
            return;
        };
        let model = message
            .model
            .as_deref()
            .filter(|model| !model.is_empty() && *model != "<synthetic>")
            .unwrap_or("unknown")
            .to_owned();
        let mode = mode(usage_record);
        // Empty placeholders must not create unknown model or mode rows.
        if usage.total() == 0 && (model == "unknown" || mode == UsageMode::Unknown) {
            return;
        }
        let event = UsageEvent {
            id: key.clone(),
            session: session.clone(),
            timestamp,
            model,
            provider: "unknown".into(),
            tier: None,
            mode,
            project: record.cwd.clone(),
            usage,
            cache_writes: Some(breakdown),
            source: self.source.clone(),
            origins: BTreeSet::from([self.source.clone()]),
            context_tokens: None,
        };
        if let Some(info) = self.sessions.get_mut(&session) {
            info.origins.insert(self.source.clone());
            if info.project.is_none() {
                info.project.clone_from(&record.cwd);
            }
        }
        self.candidates.push(Candidate {
            id: Some(key),
            event: Some(event),
            file: self.path.clone(),
            session,
            owner_conflict,
        });
    }

    fn infer_owner(&mut self, at: RecordLocation<'_>) {
        if !self.inferred_owner_reported {
            self.inferred_owner_reported = true;
            self.problem(
                at,
                "assistant record omits ownership metadata; recognized filename used",
                false,
            );
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            candidates: self.candidates.clone(),
            sessions: self.sessions.clone(),
        }
    }
}

#[derive(Default)]
struct Diagnostics {
    global: Coverage,
    per_source: BTreeMap<String, Coverage>,
}
impl Diagnostics {
    fn duplicate(&mut self, origins: &BTreeSet<String>, count: usize) {
        self.global.duplicates += count;
        for source in origins {
            self.per_source
                .entry(source.clone())
                .or_default()
                .duplicates += count;
        }
    }
    fn bump(&mut self, origins: &BTreeSet<String>, message: &str, conflict: bool, excluded: bool) {
        let touch = |coverage: &mut Coverage| {
            if conflict {
                coverage.conflicts += 1;
            } else {
                coverage.uncertain += 1;
            }
            if excluded {
                coverage.excluded_usage += 1;
            }
        };
        touch(&mut self.global);
        for source in origins {
            touch(self.per_source.entry(source.clone()).or_default());
        }
        note(&mut self.global, message);
    }
}

pub(crate) fn reconcile(
    snapshots: &[ParsedFileSnapshot],
    stopped: &AtomicBool,
) -> Result<AgentScan> {
    cancel::check(stopped)?;
    let mut candidates = Vec::new();
    let mut sessions = BTreeMap::new();
    for snapshot in snapshots {
        let ParsedFileSnapshot::ClaudeCode(snapshot) = snapshot else {
            continue;
        };
        for (key, info) in &snapshot.sessions {
            sessions
                .entry(key.clone())
                .or_insert_with(|| SessionInfo::new(key.clone()))
                .merge(info);
        }
        candidates.extend(snapshot.candidates.iter().cloned());
    }
    let observed_event_ids: BTreeSet<EventKey> = candidates
        .iter()
        .filter_map(|candidate| candidate.id.clone())
        .collect();
    for candidate in &candidates {
        let info = sessions
            .entry(candidate.session.clone())
            .or_insert_with(|| SessionInfo::new(candidate.session.clone()));
        info.origins.extend(
            candidate
                .event
                .as_ref()
                .map(|event| event.origins.clone())
                .unwrap_or_default(),
        );
        if let Some(event) = &candidate.event {
            if info.project.is_none() {
                info.project.clone_from(&event.project);
            }
        }
        if !info.files.iter().any(|file| file.path == candidate.file) {
            info.files.push(SessionFile {
                source: candidate
                    .event
                    .as_ref()
                    .map(|event| event.source.clone())
                    .unwrap_or_default(),
                path: candidate.file.clone(),
                layout: if candidate.session.execution_id.is_some() {
                    "claude-subagent"
                } else {
                    "claude-main"
                }
                .into(),
            });
        }
    }

    let mut diagnostics = Diagnostics::default();
    let mut groups: BTreeMap<EventKey, Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates {
        if let Some(id) = &candidate.id {
            groups.entry(id.clone()).or_default().push(candidate);
        }
    }
    let mut events = Vec::new();
    for (id, group) in groups {
        cancel::check(stopped)?;
        let origins: BTreeSet<String> = group
            .iter()
            .filter_map(|candidate| candidate.event.as_ref())
            .flat_map(|event| event.origins.iter().cloned())
            .collect();
        let mut usable: Vec<&Candidate> = group
            .iter()
            .filter(|candidate| candidate.event.is_some() && !candidate.owner_conflict)
            .collect();
        let owner_conflict = group.iter().any(|candidate| candidate.owner_conflict);
        if usable.is_empty() {
            // Preserve the excluded identity so later repairs are not "new" activity.
            diagnostics.bump(
                &origins,
                if owner_conflict {
                    "Claude message copies disagree on session ownership; usage excluded"
                } else {
                    "Claude message has no usable usage copy; usage excluded"
                },
                owner_conflict,
                true,
            );
            continue;
        }
        // Contradictory ownership cannot be resolved by a complete usage copy.
        // Exclude the whole group rather than charging whichever copy sorts first.
        if owner_conflict {
            diagnostics.bump(
                &origins,
                "Claude message copies disagree on session ownership; usage excluded",
                true,
                true,
            );
            continue;
        }
        if group.len() > usable.len() {
            // Incomplete copies only: the complete copy is used, with a diagnostic.
            diagnostics.bump(
                &origins,
                "Some Claude message copies had unusable accounting; a complete copy was used",
                false,
                false,
            );
        }
        // Deterministic merge order over usable copies only.
        usable.sort_by(|a, b| {
            let a_event = a.event.as_ref().expect("usable candidate has an event");
            let b_event = b.event.as_ref().expect("usable candidate has an event");
            a_event
                .timestamp
                .cmp(&b_event.timestamp)
                .then_with(|| a_event.source.cmp(&b_event.source))
                .then_with(|| a.file.cmp(&b.file))
        });
        let mut merged: UsageEvent = usable[0]
            .event
            .as_ref()
            .expect("usable candidate has an event")
            .clone();
        let mut conflict = false;
        for candidate in &usable {
            let other = candidate
                .event
                .as_ref()
                .expect("usable candidate has an event");
            if other.usage != merged.usage {
                conflict = true;
                break;
            }
            if merged.session != other.session {
                conflict = true;
                break;
            }
            if merged.model != "unknown" && other.model != "unknown" && merged.model != other.model
            {
                conflict = true;
                break;
            }
            if let (Some(first), Some(second)) = (&merged.project, &other.project) {
                if first != second {
                    conflict = true;
                    break;
                }
            }
            if let (Some(a), Some(b)) = (&merged.cache_writes, &other.cache_writes) {
                if a != b {
                    conflict = true;
                    break;
                }
            }
            if merged.timestamp > other.timestamp {
                merged.timestamp = other.timestamp;
            }
            merged.origins.extend(other.origins.iter().cloned());
            if merged.model == "unknown" && other.model != "unknown" {
                merged.model.clone_from(&other.model);
            }
            if merged.project.is_none() {
                merged.project.clone_from(&other.project);
            }
            if merged.cache_writes.is_none() {
                merged.cache_writes.clone_from(&other.cache_writes);
            }
        }
        if conflict {
            diagnostics.bump(
                &origins,
                &format!("Conflicting copies of Claude message {id}; usage excluded"),
                true,
                true,
            );
            continue;
        }
        merged.origins.insert(merged.source.clone());
        diagnostics.duplicate(&origins, usable.len().saturating_sub(1));
        events.push(merged);
    }
    events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
    Ok(AgentScan {
        events,
        observed_event_ids,
        sessions,
        coverage: diagnostics.global,
        source_coverage: diagnostics.per_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{Frame, JsonlCursor};
    use std::io::{BufReader, Write};
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("claude");
            fs::create_dir_all(root.join("projects/p")).unwrap();
            Self { _dir: dir, root }
        }
        fn write(&self, relative: &str, records: &[Value]) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = fs::File::create(&path).unwrap();
            for record in records {
                writeln!(file, "{record}").unwrap();
            }
            path
        }
        fn scan(&self) -> crate::model::Scan {
            let mut reader = crate::reader::Reader::new();
            reader
                .scan(&[crate::model::Source {
                    name: "claude".into(),
                    agent: AgentKind::ClaudeCode,
                    kind: "linux".into(),
                    root: self.root.clone(),
                    enabled: true,
                    pricing_provider: None,
                }])
                .unwrap()
        }
        fn events(&self) -> Vec<UsageEvent> {
            self.scan().events
        }
        fn path(&self) -> &Path {
            &self.root
        }
    }

    fn assistant(
        message: &str,
        session: &str,
        time: &str,
        input: u64,
        read: u64,
        write: u64,
        output: u64,
    ) -> Value {
        serde_json::json!({
            "type":"assistant","sessionId":session,"timestamp":time,"cwd":"/project",
            "version":"2.1.318","uuid":format!("uuid-{message}-{time}"),
            "message":{"id":message,"model":"claude-synthetic","usage":{
                "input_tokens":input,"cache_read_input_tokens":read,
                "cache_creation_input_tokens":write,"output_tokens":output
            }}
        })
    }

    #[test]
    fn zero_usage_unknown_models_are_ignored_but_real_usage_is_retained() {
        let fixture = Fixture::new();
        let mut records = Vec::new();
        for (index, model) in [
            Value::Null,
            serde_json::json!(""),
            serde_json::json!("<synthetic>"),
            serde_json::json!("unknown"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut record = assistant(
                &format!("placeholder-{index}"),
                "session-1",
                "2026-11-01T10:00:00Z",
                0,
                0,
                0,
                0,
            );
            record["message"]["model"] = model.clone();
            records.push(record);
            let mut used = assistant(
                &format!("used-{index}"),
                "session-1",
                "2026-11-01T10:00:00Z",
                0,
                1,
                0,
                0,
            );
            used["message"]["model"] = model;
            records.push(used);
        }
        records.push(assistant(
            "known",
            "session-1",
            "2026-11-01T10:00:00Z",
            0,
            0,
            0,
            0,
        ));
        fixture.write("projects/p/session-1.jsonl", &records);
        let events = fixture.events();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.model == "unknown")
                .count(),
            4
        );
        assert_eq!(
            events.iter().map(|event| event.usage.total()).sum::<u64>(),
            4
        );
    }

    #[test]
    fn zero_usage_unknown_mode_is_ignored_but_known_mode_and_real_usage_are_retained() {
        let fixture = Fixture::new();
        let placeholder = assistant(
            "placeholder",
            "session-1",
            "2026-11-01T10:00:00Z",
            0,
            0,
            0,
            0,
        );
        let mut regular = assistant("regular", "session-1", "2026-11-01T10:00:00Z", 0, 0, 0, 0);
        regular["message"]["usage"]["speed"] = serde_json::json!("standard");
        let used = assistant("used", "session-1", "2026-11-01T10:00:00Z", 0, 1, 0, 0);
        fixture.write("projects/p/session-1.jsonl", &[placeholder, regular, used]);
        let events = fixture.events();
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| event.mode == UsageMode::Regular));
        assert!(events
            .iter()
            .any(|event| event.mode == UsageMode::Unknown && event.usage.total() == 1));
    }

    #[test]
    fn claude_input_normalization_counts_cache_reads_and_writes_once() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[assistant(
                "msg_1",
                "session-1",
                "2026-11-01T10:00:00Z",
                100,
                800,
                200,
                50,
            )],
        );
        let events = fixture.events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.usage.input, 1100);
        assert_eq!(event.usage.total(), 1150);
        assert_eq!(event.usage.cached, Some(800));
        assert_eq!(event.usage.cache_write, Some(200));
        assert_eq!(event.session.native_id, "session-1");
        assert_eq!(event.id.native_id, "message:msg_1");
        assert_eq!(event.provider, "unknown");
        assert_eq!(event.mode, UsageMode::Unknown);
    }

    #[test]
    fn fragments_with_one_message_id_merge_to_the_earliest_timestamp() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[
                assistant("msg_1", "session-1", "2026-11-01T10:00:03Z", 10, 0, 0, 1),
                assistant("msg_1", "session-1", "2026-11-01T10:00:01Z", 10, 0, 0, 1),
                assistant("msg_1", "session-1", "2026-11-01T10:00:02Z", 10, 0, 0, 1),
            ],
        );
        let events = fixture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].timestamp,
            "2026-11-01T10:00:01Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn session_id_field_is_not_an_alias_and_filename_contradictions_are_excluded() {
        let fixture = Fixture::new();
        let mut record = assistant("msg_1", "session-1", "2026-11-01T10:00:00Z", 10, 0, 0, 1);
        record["session_id"] = serde_json::json!("different");
        fixture.write("projects/p/session-1.jsonl", &[record]);
        let scan = fixture.scan();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].session.native_id, "session-1");

        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[assistant(
                "msg_1",
                "other-session",
                "2026-11-01T10:00:00Z",
                10,
                0,
                0,
                1,
            )],
        );
        let scan = fixture.scan();
        assert!(scan.events.is_empty());
        assert_eq!(scan.coverage.excluded_usage, 1);
        assert!(scan
            .observed_event_ids
            .contains(&EventKey::new(AgentKind::ClaudeCode, "message:msg_1")));
    }

    #[test]
    fn subagents_are_distinct_executions_without_parent_usage_rows() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[assistant(
                "main",
                "session-1",
                "2026-11-01T10:00:00Z",
                10,
                0,
                0,
                1,
            )],
        );
        fixture.write(
            "projects/p/session-1/subagents/agent-a.jsonl",
            &[assistant(
                "a",
                "session-1",
                "2026-11-01T10:00:01Z",
                20,
                0,
                0,
                2,
            )],
        );
        fixture.write(
            "projects/p/session-1/subagents/agent-b.jsonl",
            &[assistant(
                "b",
                "session-1",
                "2026-11-01T10:00:02Z",
                30,
                0,
                0,
                3,
            )],
        );
        let scan = fixture.scan();
        assert_eq!(scan.events.len(), 3);
        let keys: BTreeSet<_> = scan
            .events
            .iter()
            .map(|event| event.session.clone())
            .collect();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&SessionKey::main(AgentKind::ClaudeCode, "session-1")));
        assert!(keys.contains(&SessionKey::execution(
            AgentKind::ClaudeCode,
            "session-1",
            "a"
        )));
        let parent = scan.sessions[&SessionKey::main(AgentKind::ClaudeCode, "session-1")]
            .parent
            .clone();
        assert_eq!(parent, None);
        let sub = &scan.sessions[&SessionKey::execution(AgentKind::ClaudeCode, "session-1", "a")];
        assert_eq!(
            sub.parent,
            Some(SessionKey::main(AgentKind::ClaudeCode, "session-1"))
        );
        assert_eq!(
            scan.events
                .iter()
                .filter(|event| event.session.is_main())
                .count(),
            1
        );
    }

    #[test]
    fn mirrored_roots_merge_origins_but_differing_totals_exclude_the_event() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        for dir in [left.path(), right.path()] {
            fs::create_dir_all(dir.join("projects/p")).unwrap();
        }
        let record = assistant("msg_1", "session-1", "2026-11-01T10:00:00Z", 10, 0, 0, 1);
        let write = |dir: &Path, record: &Value| {
            let mut file = fs::File::create(dir.join("projects/p/session-1.jsonl")).unwrap();
            writeln!(file, "{record}").unwrap();
        };
        write(left.path(), &record);
        write(right.path(), &record);
        let source = |root: &Path, name: &str| crate::model::Source {
            name: name.into(),
            agent: AgentKind::ClaudeCode,
            kind: "linux".into(),
            root: root.into(),
            enabled: true,
            pricing_provider: None,
        };
        let scan = crate::reader::Reader::new()
            .scan(&[source(left.path(), "a"), source(right.path(), "b")])
            .unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].origins.len(), 2);
        assert_eq!(scan.coverage.duplicates, 1);

        let mut conflicting = record.clone();
        conflicting["message"]["usage"]["output_tokens"] = serde_json::json!(9);
        write(right.path(), &conflicting);
        let scan = crate::reader::Reader::new()
            .scan(&[source(left.path(), "a"), source(right.path(), "b")])
            .unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.coverage.conflicts, 1);
        assert_eq!(scan.coverage.excluded_usage, 1);
        assert!(scan
            .observed_event_ids
            .contains(&EventKey::new(AgentKind::ClaudeCode, "message:msg_1")));
    }

    #[test]
    fn identical_counts_under_distinct_message_ids_remain_two_events() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[
                assistant("msg_1", "session-1", "2026-11-01T10:00:00Z", 10, 0, 0, 1),
                assistant("msg_2", "session-1", "2026-11-01T10:00:01Z", 10, 0, 0, 1),
            ],
        );
        assert_eq!(fixture.events().len(), 2);
    }

    #[test]
    fn missing_identity_counts_and_breakdowns_are_explicit() {
        let fixture = Fixture::new();
        let mut no_id = assistant("msg_1", "session-1", "2026-11-01T10:00:00Z", 10, 0, 0, 1);
        no_id["message"].as_object_mut().unwrap().remove("id");
        let mut missing = assistant("msg_2", "session-1", "2026-11-01T10:00:01Z", 10, 0, 0, 1);
        missing["message"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("input_tokens");
        let mut ttl = assistant("msg_3", "session-1", "2026-11-01T10:00:02Z", 10, 0, 200, 1);
        ttl["message"]["usage"]["cache_creation"] = serde_json::json!({
            "ephemeral_5m_input_tokens":150,"ephemeral_1h_input_tokens":40
        });
        fixture.write("projects/p/session-1.jsonl", &[no_id, missing, ttl]);
        let scan = fixture.scan();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].usage.cache_write, Some(200));
        assert_eq!(
            scan.events[0].cache_writes,
            Some(CacheWriteBreakdown::unknown(200))
        );
        // One unidentified record plus one identified group with no usable copy.
        assert_eq!(scan.coverage.excluded_usage, 2);
        assert!(scan.coverage.incomplete());
        // Both identified records are observed; the unidentified one has no key.
        assert_eq!(scan.observed_event_ids.len(), 2);
        for id in ["message:msg_2", "message:msg_3"] {
            assert!(scan
                .observed_event_ids
                .contains(&EventKey::new(AgentKind::ClaudeCode, id)));
        }
    }

    #[test]
    fn complete_copy_beside_incomplete_copies_counts_once_without_panicking() {
        let complete = assistant("msg_1", "session-1", "2026-11-01T10:00:00Z", 100, 0, 0, 50);
        let mut missing_usage =
            assistant("msg_1", "session-1", "2026-11-01T10:00:01Z", 100, 0, 0, 50);
        missing_usage["message"]
            .as_object_mut()
            .unwrap()
            .remove("usage");
        let mut missing_count =
            assistant("msg_1", "session-1", "2026-11-01T10:00:02Z", 100, 0, 0, 50);
        missing_count["message"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("input_tokens");
        let mut missing_time =
            assistant("msg_1", "session-1", "2026-11-01T10:00:03Z", 100, 0, 0, 50);
        missing_time.as_object_mut().unwrap().remove("timestamp");

        for (label, records, cross_file) in [
            (
                "complete-then-incomplete",
                vec![complete.clone(), missing_usage.clone()],
                false,
            ),
            (
                "incomplete-then-complete",
                vec![missing_usage.clone(), complete.clone()],
                false,
            ),
            (
                "invalid-counts",
                vec![complete.clone(), missing_count.clone()],
                false,
            ),
            (
                "missing-timestamp",
                vec![complete.clone(), missing_time.clone()],
                false,
            ),
            (
                "across-files",
                vec![complete.clone(), missing_usage.clone()],
                true,
            ),
        ] {
            let fixture = Fixture::new();
            if cross_file {
                fixture.write("projects/p/session-1.jsonl", &records[..1]);
                fixture.write("projects/q/session-1.jsonl", &records[1..]);
            } else {
                fixture.write("projects/p/session-1.jsonl", &records);
            }
            let scan = fixture.scan();
            assert_eq!(scan.events.len(), 1, "{label}");
            assert_eq!(scan.events[0].usage.total(), 150, "{label}");
            assert_eq!(scan.coverage.conflicts, 0, "{label}");
            assert_eq!(scan.coverage.excluded_usage, 0, "{label}");
            assert!(scan.coverage.uncertain >= 1, "{label}");
            assert!(scan
                .observed_event_ids
                .contains(&EventKey::new(AgentKind::ClaudeCode, "message:msg_1")));
        }
    }

    #[test]
    fn contradictory_ownership_excludes_the_group_even_with_a_complete_copy() {
        let valid = assistant("msg_1", "session-1", "2026-11-01T10:00:00Z", 100, 0, 0, 50);
        // The record says session-1, but the recognized filename says "other".
        let contradictory = assistant("msg_1", "session-1", "2026-11-01T10:00:01Z", 100, 0, 0, 50);
        for (label, first, second) in [
            ("valid-first", valid.clone(), contradictory.clone()),
            ("conflict-first", contradictory, valid),
        ] {
            let fixture = Fixture::new();
            fixture.write("projects/p/session-1.jsonl", &[first]);
            fixture.write("projects/p/other.jsonl", &[second]);
            let scan = fixture.scan();
            assert!(scan.events.is_empty(), "{label}");
            assert_eq!(scan.coverage.conflicts, 1, "{label}");
            assert_eq!(scan.coverage.excluded_usage, 1, "{label}");
            assert!(scan
                .observed_event_ids
                .contains(&EventKey::new(AgentKind::ClaudeCode, "message:msg_1")));
        }
    }

    #[test]
    fn ttl_breakdown_maps_verified_fields() {
        let fixture = Fixture::new();
        let mut record = assistant(
            "msg_1",
            "session-1",
            "2026-11-01T10:00:00Z",
            100,
            0,
            200,
            10,
        );
        record["message"]["usage"]["cache_creation"] = serde_json::json!({
            "ephemeral_5m_input_tokens":150,"ephemeral_1h_input_tokens":50
        });
        fixture.write("projects/p/session-1.jsonl", &[record]);
        let event = &fixture.events()[0];
        let breakdown = event.cache_writes.as_ref().unwrap();
        assert_eq!(breakdown.by_ttl_seconds.get(&300), Some(&150));
        assert_eq!(breakdown.by_ttl_seconds.get(&3600), Some(&50));
        assert_eq!(breakdown.unknown_ttl, 0);
        assert_eq!(event.usage.input, 300);
    }

    #[test]
    fn malformed_json_and_unrecognized_files_do_not_become_usage() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "projects/p/session-1.jsonl",
            &[assistant(
                "msg_1",
                "session-1",
                "2026-11-01T10:00:00Z",
                10,
                0,
                0,
                1,
            )],
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "not json").unwrap();
        fs::write(fixture.path().join("history.jsonl"), "ignored").unwrap();
        fs::create_dir_all(fixture.path().join("projects/p/other")).unwrap();
        fs::write(
            fixture.path().join("projects/p/other/random.txt"),
            "ignored",
        )
        .unwrap();
        let scan = fixture.scan();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.coverage.malformed, 1);
        let _ = JsonlCursor::default();
        let _ = Frame::Line(b"");
        let _ = BufReader::new(std::io::empty());
    }
}
