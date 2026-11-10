//! Codex rollout parsing, quota observations, and usage reconciliation.
//!
//! Native event identity is unchanged from the original Codex-only package:
//! `response:<id>`, `legacy:<Millis>:<snapshot>`, or a content hash for records
//! without stable response identity. Only the surrounding typed identity changed.
use super::{note, string, timestamp, AgentScan, DiscoveredFile, ParsedFileSnapshot};
use crate::{
    cancel,
    model::{
        AgentKind, CacheWriteBreakdown, Coverage, EventKey, SessionFile, SessionInfo, SessionKey,
        Usage, UsageEvent, UsageMode,
    },
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use walkdir::WalkDir;

#[derive(Clone, Default)]
struct Turn {
    model: String,
    project: Option<String>,
    provider: String,
    tier: Option<String>,
}
#[derive(Clone)]
pub(crate) struct Candidate {
    pub(crate) event: UsageEvent,
    pub(crate) file: PathBuf,
    turn: Option<String>,
    snapshot: Option<String>,
    direct: bool,
}

#[derive(Clone)]
pub(crate) struct Parser {
    source: String,
    path: PathBuf,
    owner: String,
    active_session: String,
    owner_created: Option<DateTime<Utc>>,
    parent: Option<String>,
    current: Turn,
    current_turn: Option<String>,
    turns: HashMap<String, Turn>,
    baselines: HashMap<String, Usage>,
    baseline_models: HashMap<String, String>,
    events: Vec<Candidate>,
    pub(crate) quota_snapshots: Vec<crate::quota::QuotaSnapshot>,
    pub(crate) coverage: Coverage,
    line: u64,
    direct_responses: BTreeSet<String>,
    compactions: BTreeSet<String>,
    fallback_counts: HashMap<String, u64>,
    recent_direct: HashMap<String, (Usage, DateTime<Utc>, bool)>,
    thread_tiers: HashMap<String, Option<String>>,
    sessions: BTreeMap<SessionKey, SessionInfo>,
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) sessions: BTreeMap<SessionKey, SessionInfo>,
}

fn mode_from_tier(tier: Option<&str>) -> UsageMode {
    match tier {
        Some("default" | "standard") => UsageMode::Regular,
        Some("priority" | "fast") => UsageMode::Fast,
        Some("flex") => UsageMode::Flex,
        Some("ultrafast") => UsageMode::Ultrafast,
        Some("batch") => UsageMode::Batch,
        _ => UsageMode::Unknown,
    }
}

fn cache_breakdown(writes: Option<u64>, retention: Option<&str>) -> Option<CacheWriteBreakdown> {
    let writes = writes?;
    match retention {
        Some("30m" | "30min") => Some(CacheWriteBreakdown::from_ttl(1800, writes)),
        Some("12h") => Some(CacheWriteBreakdown::from_ttl(43_200, writes)),
        // A recorded but unrecognized duration has no default rate.
        Some(_) => Some(CacheWriteBreakdown::unknown(writes)),
        // `None` preserves legacy Codex retention assumptions in pricing.
        None => None,
    }
}

impl Parser {
    pub(crate) fn new(source: &str, path: &Path) -> Self {
        Self {
            source: source.into(),
            path: path.to_path_buf(),
            owner: String::new(),
            active_session: String::new(),
            owner_created: None,
            parent: None,
            current: Turn::default(),
            current_turn: None,
            turns: HashMap::new(),
            baselines: HashMap::new(),
            baseline_models: HashMap::new(),
            events: Vec::new(),
            quota_snapshots: Vec::new(),
            coverage: Coverage::default(),
            line: 0,
            direct_responses: BTreeSet::new(),
            compactions: BTreeSet::new(),
            fallback_counts: HashMap::new(),
            recent_direct: HashMap::new(),
            thread_tiers: HashMap::new(),
            sessions: BTreeMap::new(),
        }
    }

    fn session_info(&mut self, session: &str) -> &mut SessionInfo {
        let key = SessionKey::main(AgentKind::Codex, session);
        let info = self
            .sessions
            .entry(key.clone())
            .or_insert_with(|| SessionInfo::new(key));
        info.origins.insert(self.source.clone());
        if !info
            .files
            .iter()
            .any(|file| file.source == self.source && file.path == self.path)
        {
            info.files.push(SessionFile {
                source: self.source.clone(),
                path: self.path.clone(),
                layout: "codex-rollout".into(),
            });
        }
        info
    }

    pub(crate) fn ingest(&mut self, bytes: &[u8], at: super::RecordLocation<'_>) {
        self.line = at.line;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        // Rollout bodies can contain megabytes of private text. Only deserialize usage/metadata.
        if let Some(kind) = head_kind(bytes) {
            if !matches!(
                kind.as_str(),
                "session_meta"
                    | "turn_context"
                    | "token_usage_record"
                    | "event_msg"
                    | "thread_settings_applied"
                    | "compacted"
            ) {
                if kind.contains("usage") || kind.contains("token") {
                    self.problem("Unrecognized usage record type", false);
                }
                return;
            }
            if kind == "event_msg"
                && !bytes.windows(13).any(|part| part == b"\"token_count\"")
                && !bytes
                    .windows(25)
                    .any(|part| part == b"\"thread_settings_applied\"")
            {
                return;
            }
        }
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(_) => {
                self.problem("Malformed JSON record", true);
                return;
            }
        };
        self.parse_value(&value);
    }

    fn parse_value(&mut self, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = &value["payload"];
        match kind {
            "session_meta" => {
                let session = string(payload, "id")
                    .or_else(|| string(payload, "thread_id"))
                    .or_else(|| string(payload, "session_id"))
                    .unwrap_or_default();
                if self.owner.is_empty() {
                    self.owner = session.clone();
                    self.owner_created = timestamp(payload).or_else(|| timestamp(value));
                    self.parent = string(payload, "forked_from_id")
                        .or_else(|| string(payload, "parent_thread_id"));
                }
                if !session.is_empty() {
                    self.active_session = session.clone();
                    self.current.tier = self
                        .thread_tiers
                        .get(&self.active_session)
                        .cloned()
                        .flatten();
                    let project = string(payload, "cwd");
                    let parent = self.parent.clone();
                    let info = self.session_info(&session);
                    if info.project.is_none() {
                        info.project = project;
                    }
                    if info.parent.is_none() {
                        info.parent =
                            parent.map(|parent| SessionKey::main(AgentKind::Codex, parent));
                    }
                }
                self.current.provider =
                    string(payload, "model_provider").unwrap_or_else(|| "unknown".into());
                self.current.project = string(payload, "cwd");
            }
            "turn_context" => {
                self.current_turn = string(payload, "turn_id");
                if let (Some(parent), Some(created), Some(turn)) = (
                    self.parent.as_ref(),
                    self.owner_created,
                    self.current_turn.as_deref().and_then(uuid_v7_millis),
                ) {
                    let session = if turn < created.timestamp_millis() {
                        parent
                    } else {
                        &self.owner
                    };
                    if self.active_session != *session {
                        self.active_session.clone_from(session);
                        self.current.tier = self.thread_tiers.get(session).cloned().flatten();
                    }
                }
                if let Some(model) = string(payload, "model") {
                    self.current.model = model;
                }
                if let Some(provider) = string(payload, "model_provider") {
                    self.current.provider = provider;
                }
                if let Some(project) = string(payload, "cwd") {
                    self.current.project = Some(project);
                }
                if let Some(tier) = recorded_tier(payload) {
                    self.current.tier = tier.clone();
                    self.thread_tiers.insert(self.active_session.clone(), tier);
                }
                if let Some(id) = &self.current_turn {
                    self.turns.insert(id.clone(), self.current.clone());
                }
            }
            "thread_settings_applied" => self.settings(payload),
            "compacted" => {
                if let Some(response) = string(payload, "response_id") {
                    self.compactions.insert(response);
                }
            }
            "token_usage_record" => self.direct(value),
            "event_msg" if payload["type"] == "thread_settings_applied" => self.settings(payload),
            "event_msg" if payload["type"] == "token_count" => {
                if let Some(time) = timestamp(value) {
                    self.quota_snapshots
                        .extend(crate::quota::QuotaSnapshot::parse(
                            time,
                            &payload["rate_limits"],
                            &self.source,
                        ));
                }
                self.legacy(value);
            }
            _ => {}
        }
    }

    pub(crate) fn malformed_final(&mut self, at: super::RecordLocation<'_>) {
        self.line = at.line;
        self.problem("Malformed final JSON record", true);
    }

    pub(crate) fn missing_compactions(&self) -> usize {
        self.compactions.difference(&self.direct_responses).count()
    }

    fn problem(&mut self, message: &str, malformed: bool) {
        if malformed {
            self.coverage.malformed += 1;
            // Known usage-bearing data that cannot be accounted for.
            self.coverage.excluded_usage += 1;
        } else {
            self.coverage.uncertain += 1;
        }
        note(
            &mut self.coverage,
            format!("{}:{}: {message}", self.path.display(), self.line),
        );
    }

    fn settings(&mut self, payload: &Value) {
        let tier = recorded_tier(payload).unwrap_or(None);
        let session = string(payload, "thread_id").unwrap_or_else(|| self.active_session.clone());
        self.thread_tiers.insert(session.clone(), tier.clone());
        if session == self.active_session {
            self.current.tier = tier;
            if let Some(id) = &self.current_turn {
                if let Some(turn) = self.turns.get_mut(id) {
                    turn.tier.clone_from(&self.current.tier);
                }
            }
        }
    }

    fn event(
        &self,
        id: String,
        time: DateTime<Utc>,
        session: String,
        usage: Usage,
        payload: &Value,
    ) -> UsageEvent {
        let retention = string(payload, "cache_retention")
            .or_else(|| string(payload, "prompt_cache_retention"));
        let cache_writes = cache_breakdown(usage.cache_write, retention.as_deref());
        let tier = recorded_tier(payload).unwrap_or_else(|| self.current.tier.clone());
        UsageEvent {
            id: EventKey::new(AgentKind::Codex, id),
            timestamp: time,
            session: SessionKey::main(AgentKind::Codex, session),
            model: if self.current.model.is_empty() {
                "unknown".into()
            } else {
                self.current.model.clone()
            },
            provider: if self.current.provider.is_empty() {
                "unknown".into()
            } else {
                self.current.provider.clone()
            },
            mode: mode_from_tier(tier.as_deref()),
            tier,
            project: self.current.project.clone(),
            usage,
            cache_writes,
            source: self.source.clone(),
            origins: BTreeSet::from([self.source.clone()]),
            context_tokens: payload.get("context_tokens").and_then(Value::as_u64),
        }
    }

    fn direct(&mut self, value: &Value) {
        let payload = &value["payload"];
        let Some(time) = timestamp(value) else {
            self.problem("Usage record has no valid timestamp", true);
            return;
        };
        let usage = match parse_usage(&payload["usage"]) {
            Ok(usage) => usage,
            Err(error) => {
                self.problem(&error, true);
                return;
            }
        };
        let session = string(payload, "thread_id")
            .or_else(|| string(payload, "session_id"))
            .unwrap_or_else(|| self.active_session.clone());
        if session.is_empty() {
            self.coverage.excluded_usage += 1;
            self.problem("Usage record has no session identity", false);
            return;
        }
        if self.active_session != session {
            self.current.tier = self.thread_tiers.get(&session).cloned().flatten();
            self.active_session.clone_from(&session);
        }
        let response = string(payload, "response_id");
        let id = if let Some(response) = &response {
            self.direct_responses.insert(response.clone());
            format!("response:{response}")
        } else {
            self.problem("Per-response usage lacks stable response identity", false);
            let hash = blake3::hash(value.to_string().as_bytes())
                .to_hex()
                .to_string();
            let occurrence = self.fallback_counts.entry(hash.clone()).or_default();
            *occurrence += 1;
            format!("record:{hash}:{occurrence}")
        };
        if payload.get("thread_id").is_none()
            && payload.get("turn_id").is_none()
            && payload.get("thread_token_usage").is_none()
        {
            self.coverage.excluded_usage += 1;
            self.problem("Standalone usage lacks turn/counter evidence for compaction reconciliation; excluded", false);
            return;
        }
        let cumulative = parse_usage(&payload["thread_token_usage"]).ok();
        let snapshot = cumulative
            .as_ref()
            .map(|total| snapshot_key(&session, total));
        if self.parent.is_some() && session != self.owner {
            // The source explicitly records another thread: this is replayed parent history.
            if let Some(total) = cumulative {
                self.baselines.insert(session, total);
            }
            self.coverage.duplicates += 1;
            return;
        }
        let turn = string(payload, "turn_id").or_else(|| self.current_turn.clone());
        let mut event = self.event(id, time, session.clone(), usage, payload);
        if turn != self.current_turn {
            if let Some(context) = turn.as_ref().and_then(|id| self.turns.get(id)) {
                event.model = if context.model.is_empty() {
                    "unknown".into()
                } else {
                    context.model.clone()
                };
                event.project.clone_from(&context.project);
                event.provider = if context.provider.is_empty() {
                    "unknown".into()
                } else {
                    context.provider.clone()
                };
                event.tier.clone_from(&context.tier);
            } else {
                event.model = "unknown".into();
            }
        }
        if let Some(tier) = recorded_tier(payload) {
            // Per-response evidence wins over thread defaults or inferred active context.
            event.tier = tier;
        }
        event.mode = mode_from_tier(event.tier.as_deref());
        self.recent_direct
            .insert(session, (event.usage.clone(), time, false));
        self.events.push(Candidate {
            event,
            file: self.path.clone(),
            turn,
            snapshot,
            direct: true,
        });
    }

    fn legacy(&mut self, value: &Value) {
        let payload = &value["payload"];
        let info = &payload["info"];
        if info.is_null() {
            return;
        }
        let Some(time) = timestamp(value) else {
            self.problem("Token snapshot has no valid timestamp", true);
            return;
        };
        let total = match parse_usage(&info["total_token_usage"]) {
            Ok(total) => total,
            Err(error) => {
                self.problem(&error, true);
                return;
            }
        };
        if self.parent.is_some() && self.inherited_turn(time) {
            // Replayed historical records may be re-timestamped during child creation.
            // The stable UUIDv7 turn identity, not the copied record timestamp, locates them.
            let parent = self.parent.clone().unwrap();
            self.baselines.insert(parent.clone(), total);
            self.baseline_models
                .insert(parent, self.current.model.clone());
            self.coverage.duplicates += 1;
            return;
        }
        let session =
            if self.parent.is_some() && self.owner_created.is_some_and(|created| time >= created) {
                self.owner.clone()
            } else if self.parent.is_some() && self.active_session == self.owner {
                self.parent.clone().unwrap()
            } else {
                self.active_session.clone()
            };
        if session.is_empty() {
            self.coverage.excluded_usage += 1;
            self.problem("Token snapshot has no session identity", false);
            return;
        }
        let latest = parse_usage(&info["last_token_usage"]).ok();
        let legacy_delta = self
            .baselines
            .get(&session)
            .and_then(|previous| subtract_usage(&total, previous));
        if let Some((usage, observed, consumed)) = self.recent_direct.get_mut(&session) {
            if !*consumed
                && (latest.as_ref() == Some(usage) || legacy_delta.as_ref() == Some(usage))
                && (time - *observed).num_milliseconds().unsigned_abs() <= 2000
            {
                *consumed = true;
                // Compatibility counters can exclude compaction that the new counters include.
                // Keep the two counter series separate; this request was already counted directly.
                self.baselines.insert(session.clone(), total);
                self.baseline_models
                    .insert(session, self.current.model.clone());
                self.coverage.duplicates += 1;
                return;
            }
        }
        let previous = self.baselines.get(&session).cloned().or_else(|| {
            if session == self.owner {
                self.parent
                    .as_ref()
                    .and_then(|parent| self.baselines.get(parent))
                    .cloned()
            } else {
                None
            }
        });
        let usage = if let Some(previous) = &previous {
            match subtract_usage(&total, previous) {
                Some(delta) => delta,
                None => {
                    self.baselines.insert(session, total);
                    self.coverage.excluded_usage += 1;
                    self.problem("Cumulative counter decreased; baseline reset, ambiguous increment excluded", false);
                    return;
                }
            }
        } else {
            total.clone()
        };
        self.baselines.insert(session.clone(), total.clone());
        let previous_model = self
            .baseline_models
            .insert(session.clone(), self.current.model.clone());
        if usage.total() == 0 {
            return;
        }
        if let Err(error) = usage.validate() {
            self.problem(&error.to_string(), true);
            return;
        }
        let snapshot = snapshot_key(&session, &total);
        let id = format!("legacy:{}:{}", time.timestamp_millis(), snapshot);
        let mut event = self.event(id, time, session, usage, info);
        let mixed_model = previous_model.is_some_and(|model| model != self.current.model)
            && !parse_usage(&info["last_token_usage"]).is_ok_and(|last| last == event.usage);
        if mixed_model {
            event.model = "unknown".into();
            self.problem(
                "Cumulative delta spans a model change; model attribution is unknown",
                false,
            );
        }
        event.mode = mode_from_tier(event.tier.as_deref());
        self.events.push(Candidate {
            event,
            file: self.path.clone(),
            turn: if mixed_model {
                None
            } else {
                self.current_turn.clone()
            },
            snapshot: Some(snapshot),
            direct: false,
        });
    }

    fn inherited_turn(&self, record_time: DateTime<Utc>) -> bool {
        let Some(created) = self.owner_created else {
            return false;
        };
        if let Some(turn_time) = self.current_turn.as_deref().and_then(uuid_v7_millis) {
            return turn_time < created.timestamp_millis();
        }
        record_time < created
    }

    /// Apply cross-record turn metadata exactly as the original reader did, then
    /// freeze the state for cross-file reconciliation.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let mut candidates = self.events.clone();
        for candidate in &mut candidates {
            if let Some(turn) = candidate.turn.as_ref().and_then(|id| self.turns.get(id)) {
                if candidate.event.model == "unknown" && !turn.model.is_empty() {
                    candidate.event.model = turn.model.clone();
                }
                if candidate.event.provider == "unknown" && !turn.provider.is_empty() {
                    candidate.event.provider = turn.provider.clone();
                }
                if candidate.event.project.is_none() && turn.project.is_some() {
                    candidate.event.project.clone_from(&turn.project);
                }
            }
            candidate.event.mode = mode_from_tier(candidate.event.tier.as_deref());
        }
        Snapshot {
            candidates,
            sessions: self.sessions.clone(),
        }
    }
}

pub(crate) fn discover(root: &Path, stopped: &AtomicBool) -> Result<Vec<DiscoveredFile>> {
    cancel::check(stopped)?;
    fs::read_dir(root).with_context(|| format!("Cannot read source {}", root.display()))?;
    let directories: Vec<_> = [root.join("sessions"), root.join("archived_sessions")]
        .into_iter()
        .filter(|path| path.is_dir())
        .collect();
    let directories = if directories.is_empty() {
        vec![root.to_path_buf()]
    } else {
        directories
    };
    let mut selected: BTreeMap<PathBuf, (usize, PathBuf)> = BTreeMap::new();
    for (directory_index, directory) in directories.iter().enumerate() {
        for entry in WalkDir::new(directory).follow_links(false) {
            cancel::check(stopped)?;
            let entry =
                entry.with_context(|| format!("Cannot enumerate {}", directory.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "history.jsonl") {
                continue;
            }
            let name = path.to_string_lossy();
            let compressed = name.ends_with(".jsonl.zst");
            if !compressed && !name.ends_with(".jsonl") {
                continue;
            }
            let mut relative = path.strip_prefix(directory).unwrap_or(path).to_path_buf();
            if compressed {
                relative.set_extension("");
            }
            let priority = directory_index * 2 + usize::from(compressed);
            let choice = selected
                .entry(relative)
                .or_insert((priority, path.to_path_buf()));
            if priority < choice.0 {
                *choice = (priority, path.to_path_buf());
            }
        }
    }
    cancel::check(stopped)?;
    Ok(selected
        .into_values()
        .map(|(_, path)| DiscoveredFile::codex(path))
        .collect())
}

fn uuid_v7_millis(id: &str) -> Option<i64> {
    let bytes = id.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[14] != b'7'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
    {
        return None;
    }
    if bytes
        .iter()
        .enumerate()
        .any(|(i, b)| ![8, 13, 18, 23].contains(&i) && !b.is_ascii_hexdigit())
    {
        return None;
    }
    i64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

fn recorded_tier(payload: &Value) -> Option<Option<String>> {
    for record in [payload, &payload["settings"], &payload["thread_settings"]] {
        if let Some(value) = record.get("service_tier") {
            return Some(value.as_str().map(str::to_owned));
        }
    }
    None
}

fn head_kind(bytes: &[u8]) -> Option<String> {
    let prefix = bytes.get(..bytes.len().min(256))?;
    let mut position = 0;
    let mut depth = 0usize;
    while position < prefix.len() {
        match prefix[position] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'"' => {
                let start = position;
                position += 1;
                while position < prefix.len() {
                    if prefix[position] == b'\\' {
                        position += 2;
                        continue;
                    }
                    if prefix[position] == b'"' {
                        break;
                    }
                    position += 1;
                }
                if depth == 1 && prefix.get(start..=position) == Some(b"\"type\"") {
                    let mut rest = prefix.get(position + 1..)?;
                    while rest.first().is_some_and(u8::is_ascii_whitespace) {
                        rest = &rest[1..];
                    }
                    if rest.first() != Some(&b':') {
                        return None;
                    }
                    rest = &rest[1..];
                    while rest.first().is_some_and(u8::is_ascii_whitespace) {
                        rest = &rest[1..];
                    }
                    if rest.first() != Some(&b'"') {
                        return None;
                    }
                    let end = rest[1..].iter().position(|byte| *byte == b'"')?;
                    return std::str::from_utf8(&rest[1..end + 1])
                        .ok()
                        .map(str::to_owned);
                }
            }
            _ => {}
        }
        position += 1;
    }
    None
}
pub(crate) fn parse_usage(value: &Value) -> std::result::Result<Usage, String> {
    let required = |key| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("Missing or invalid {key}"))
    };
    let optional = |key| match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("Invalid {key}")),
    };
    let usage = Usage {
        input: required("input_tokens")?,
        output: required("output_tokens")?,
        cached: optional("cached_input_tokens")?,
        cache_write: optional("cache_write_input_tokens")?,
        reasoning: optional("reasoning_output_tokens")?,
    };
    usage.validate().map_err(|error| error.to_string())?;
    if let Some(reported) = value.get("total_tokens") {
        if reported.as_u64() != Some(usage.total()) {
            return Err("Reported total differs from input plus output".into());
        }
    }
    Ok(usage)
}
fn subtract_usage(total: &Usage, previous: &Usage) -> Option<Usage> {
    let subset = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => a.checked_sub(b).map(Some),
        _ => Some(None),
    };
    Some(Usage {
        input: total.input.checked_sub(previous.input)?,
        output: total.output.checked_sub(previous.output)?,
        cached: subset(total.cached, previous.cached)?,
        cache_write: subset(total.cache_write, previous.cache_write)?,
        reasoning: subset(total.reasoning, previous.reasoning)?,
    })
}
fn snapshot_key(session: &str, usage: &Usage) -> String {
    // Core counters identify mirrored snapshots even if an older format omitted subsets.
    format!("{session}:{}:{}", usage.input, usage.output)
}
fn same_usage(a: &UsageEvent, b: &UsageEvent) -> bool {
    a.usage.input == b.usage.input
        && a.usage.output == b.usage.output
        && compatible(a.usage.cached, b.usage.cached)
        && compatible(a.usage.cache_write, b.usage.cache_write)
        && compatible(a.usage.reasoning, b.usage.reasoning)
}
fn same_breakdown(a: Option<&CacheWriteBreakdown>, b: Option<&CacheWriteBreakdown>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => {
            a.by_ttl_seconds == b.by_ttl_seconds && a.unknown_ttl == b.unknown_ttl
        }
    }
}
fn same_event(a: &UsageEvent, b: &UsageEvent) -> bool {
    same_usage(a, b)
        && a.session == b.session
        && (a.model == "unknown" || b.model == "unknown" || a.model == b.model)
        && (a.provider == "unknown" || b.provider == "unknown" || a.provider == b.provider)
        && (a.tier.is_none()
            || b.tier.is_none()
            || a.tier == b.tier
            || (a.mode != UsageMode::Unknown && a.mode == b.mode))
        && compatible(a.context_tokens, b.context_tokens)
        && same_breakdown(a.cache_writes.as_ref(), b.cache_writes.as_ref())
}
fn compatible(a: Option<u64>, b: Option<u64>) -> bool {
    a.is_none() || b.is_none() || a == b
}
fn merge_event(existing: &mut UsageEvent, other: &UsageEvent) {
    existing.origins.extend(other.origins.iter().cloned());
    if existing.model == "unknown" && other.model != "unknown" {
        existing.model.clone_from(&other.model);
    }
    if existing.provider == "unknown" && other.provider != "unknown" {
        existing.provider.clone_from(&other.provider);
    }
    if existing.tier.is_none() {
        existing.tier.clone_from(&other.tier);
        existing.mode = mode_from_tier(existing.tier.as_deref());
    }
    if existing.usage.cached.is_none() {
        existing.usage.cached = other.usage.cached;
    }
    if existing.usage.cache_write.is_none() {
        existing.usage.cache_write = other.usage.cache_write;
    }
    if existing.usage.reasoning.is_none() {
        existing.usage.reasoning = other.usage.reasoning;
    }
    if existing.cache_writes.is_none() {
        existing.cache_writes.clone_from(&other.cache_writes);
    }
    if existing.context_tokens.is_none() {
        existing.context_tokens = other.context_tokens;
    }
    if existing.project.is_none() {
        existing.project.clone_from(&other.project);
    }
}
fn merge_mirror(diagnostics: &mut Diagnostics, existing: &mut UsageEvent, mirror: &UsageEvent) {
    if same_event(existing, mirror) {
        merge_event(existing, mirror);
        diagnostics.bump(Kind::Duplicate, &mirror.origins, "");
    } else {
        diagnostics.bump(
            Kind::Uncertain,
            &mirror.origins,
            "Conflicting compatibility metadata excluded; identified per-response usage retained",
        );
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Duplicate,
    Conflict,
    Uncertain,
}
#[derive(Default)]
struct Diagnostics {
    global: Coverage,
    per_source: BTreeMap<String, Coverage>,
}
impl Diagnostics {
    fn bump(&mut self, kind: Kind, origins: &BTreeSet<String>, message: &str) {
        let touch = |coverage: &mut Coverage| match kind {
            Kind::Duplicate => coverage.duplicates += 1,
            Kind::Conflict => coverage.conflicts += 1,
            Kind::Uncertain => coverage.uncertain += 1,
        };
        touch(&mut self.global);
        for source in origins {
            touch(self.per_source.entry(source.clone()).or_default());
        }
        if !message.is_empty() {
            note(&mut self.global, message);
        }
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
        let ParsedFileSnapshot::Codex(snapshot) = snapshot else {
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
        .map(|candidate| candidate.event.id.clone())
        .collect();
    // Session metadata is retained even for excluded usage.
    for candidate in &candidates {
        let info = sessions
            .entry(candidate.event.session.clone())
            .or_insert_with(|| SessionInfo::new(candidate.event.session.clone()));
        info.origins.extend(candidate.event.origins.iter().cloned());
        if !info.files.iter().any(|file| file.path == candidate.file) {
            info.files.push(SessionFile {
                source: candidate.event.source.clone(),
                path: candidate.file.clone(),
                layout: "codex-rollout".into(),
            });
        }
        if info.project.is_none() {
            info.project.clone_from(&candidate.event.project);
        }
    }

    // Prefer an identified response over its equivalent legacy snapshot from another copy.
    candidates.sort_by(|a, b| {
        b.direct
            .cmp(&a.direct)
            .then_with(|| a.event.source.cmp(&b.event.source))
            .then_with(|| a.event.id.cmp(&b.event.id))
    });
    cancel::check(stopped)?;
    let mut events: BTreeMap<EventKey, UsageEvent> = BTreeMap::new();
    let mut direct_snapshots = HashMap::<_, BTreeSet<(EventKey, DateTime<Utc>)>>::new();
    let mut direct_contexts = HashMap::<_, Vec<(EventKey, PathBuf)>>::new();
    let mut conflicts = BTreeSet::new();
    let mut inconsistent_timestamps = BTreeSet::new();
    let mut diagnostics = Diagnostics::default();
    for candidate in candidates {
        cancel::check(stopped)?;
        let mut event = candidate.event;
        if candidate.direct {
            if let Some(snapshot) = candidate.snapshot.as_ref() {
                direct_snapshots
                    .entry(snapshot.clone())
                    .or_default()
                    .insert((event.id.clone(), event.timestamp));
            }
            if let Some(turn) = candidate.turn.as_ref() {
                direct_contexts
                    .entry((event.session.clone(), turn.clone(), event.timestamp))
                    .or_default()
                    .push((event.id.clone(), candidate.file.clone()));
            }
        }
        if !candidate.direct {
            if let Some(ids) = candidate
                .snapshot
                .as_ref()
                .and_then(|key| direct_snapshots.get(key))
            {
                let endpoints: BTreeSet<_> = ids
                    .iter()
                    .filter(|(id, original_timestamp)| {
                        events.contains_key(id)
                            && (*original_timestamp - event.timestamp)
                                .num_milliseconds()
                                .unsigned_abs()
                                <= 2000
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                let matching: Vec<_> = endpoints
                    .iter()
                    .filter(|id| {
                        events
                            .get(*id)
                            .is_some_and(|other| same_usage(other, &event))
                    })
                    .collect();
                if matching.len() == 1 {
                    if let Some(other) = events.get_mut(matching[0]) {
                        merge_mirror(&mut diagnostics, other, &event);
                    }
                    continue;
                } else if !endpoints.is_empty() {
                    diagnostics.bump(
                        Kind::Uncertain,
                        &event.origins,
                        "Overlapping or ambiguous cumulative compatibility increment excluded; identified per-response usage retained",
                    );
                    continue;
                }
            }
            // Older compatibility streams can omit compaction from their cumulative
            // series. Different cumulative totals must not charge the same copied
            // request twice. Require the same exact timestamp, session, turn and
            // usage in a different file; near-time coincidences are insufficient.
            if let Some(turn) = candidate.turn.as_ref() {
                let matching: BTreeSet<_> = direct_contexts
                    .get(&(event.session.clone(), turn.clone(), event.timestamp))
                    .into_iter()
                    .flatten()
                    .filter(|(id, file)| {
                        *file != candidate.file
                            && events
                                .get(id)
                                .is_some_and(|other| same_usage(other, &event))
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                if matching.len() == 1 {
                    if let Some(other) = events.get_mut(matching.first().unwrap()) {
                        merge_mirror(&mut diagnostics, other, &event);
                    }
                    continue;
                } else if matching.len() > 1 {
                    diagnostics.bump(
                        Kind::Uncertain,
                        &event.origins,
                        "Ambiguous copied compatibility record excluded: multiple direct responses match its session, turn, timestamp and usage",
                    );
                    continue;
                }
            }
        }
        if conflicts.contains(&event.id) {
            continue;
        }
        if let Some(existing) = events.get_mut(&event.id) {
            if same_event(existing, &event) {
                if existing.timestamp != event.timestamp {
                    if inconsistent_timestamps.insert(event.id.clone()) {
                        diagnostics.bump(
                            Kind::Uncertain,
                            &event.origins,
                            "Identified request copies have different timestamps; earliest recorded timestamp used for date attribution",
                        );
                    }
                    existing.timestamp = existing.timestamp.min(event.timestamp);
                }
                merge_event(existing, &event);
                diagnostics.bump(Kind::Duplicate, &event.origins, "");
            } else {
                let mut origins = event.origins.clone();
                origins.extend(existing.origins.iter().cloned());
                diagnostics.bump(
                    Kind::Conflict,
                    &origins,
                    "Conflicting copies of an identified request were excluded",
                );
                conflicts.insert(event.id.clone());
                events.remove(&event.id);
            }
        } else {
            if event.origins.is_empty() {
                event.origins.insert(event.source.clone());
            }
            events.insert(event.id.clone(), event);
        }
    }
    let mut events: Vec<_> = events.into_values().collect();
    cancel::check(stopped)?;
    events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
    cancel::check(stopped)?;
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
    use crate::agents::{ParserState, RecordLocation};
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parser_counts_a_valid_final_record_without_newline_once() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"s"}})
        )
        .unwrap();
        write!(
            file,
            "{}",
            serde_json::json!({"type":"token_usage_record","timestamp":"2026-11-01T10:00:02Z",
                "payload":{"thread_id":"s","turn_id":"t","response_id":"r",
                "usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11},
                "thread_token_usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}}})
        )
        .unwrap();
        let mut parser = ParserState::new(
            AgentKind::Codex,
            "local",
            &DiscoveredFile::codex(path.clone()),
        );
        let mut cursor = crate::reader::JsonlCursor::default();
        let file = fs::File::open(&path).unwrap();
        cursor
            .consume(
                std::io::BufReader::new(file),
                &AtomicBool::new(false),
                |frame, line| {
                    if let crate::reader::Frame::Line(bytes) = frame {
                        parser.ingest(bytes, RecordLocation { path: &path, line });
                    }
                },
            )
            .unwrap();
        let ParsedFileSnapshot::Codex(snapshot) = parser.snapshot() else {
            panic!("codex parser");
        };
        assert_eq!(snapshot.candidates.len(), 1);
        assert_eq!(parser.coverage().pending, 0);
    }
}
