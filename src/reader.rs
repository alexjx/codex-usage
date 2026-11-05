//! Read-only rollout discovery, incremental parsing, and usage reconciliation.
use crate::{
    cancel,
    model::{Coverage, Scan, Source, SourceStatus, Usage, UsageEvent},
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File, Metadata},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
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
struct Candidate {
    event: UsageEvent,
    file: PathBuf,
    turn: Option<String>,
    snapshot: Option<String>,
    direct: bool,
}
#[derive(Clone, Default)]
struct Parser {
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
    quota_snapshots: Vec<crate::quota::QuotaSnapshot>,
    coverage: Coverage,
    offset: u64,
    pending: Vec<u8>,
    line: u64,
    direct_responses: BTreeSet<String>,
    compactions: BTreeSet<String>,
    fallback_counts: HashMap<String, u64>,
    recent_direct: HashMap<String, (Usage, DateTime<Utc>, bool)>,
    thread_tiers: HashMap<String, Option<String>>,
}
#[derive(Clone)]
struct CachedFile {
    len: u64,
    modified: Option<SystemTime>,
    identity: String,
    prefix: Vec<u8>,
    parser: Parser,
}
#[derive(Clone, Default)]
struct SourceSnapshot {
    candidates: Vec<Candidate>,
    quota_snapshots: Vec<crate::quota::QuotaSnapshot>,
    coverage: Coverage,
}
type ReconcileJob = (
    (String, PathBuf),
    std::thread::JoinHandle<Result<CachedFile>>,
);
#[derive(Default)]
pub struct Reader {
    stopped: Arc<AtomicBool>,
    files: HashMap<(String, PathBuf), CachedFile>,
    sources: HashMap<String, (SourceSnapshot, DateTime<Utc>)>,
    reconcile_at: Option<Instant>,
    sweep: VecDeque<(String, PathBuf)>,
    reconcile_job: Option<ReconcileJob>,
    background_errors: BTreeMap<(String, PathBuf), String>,
}
impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cancel(stopped: Arc<AtomicBool>) -> Self {
        Self {
            stopped,
            ..Self::default()
        }
    }

    pub fn scan(&mut self, sources: &[Source]) -> Result<Scan> {
        self.scan_since(sources, None)
    }

    /// Skip files last modified more than two days before the requested period.
    /// Resumed sessions and files with unavailable modification times are included.
    pub fn scan_since(&mut self, sources: &[Source], since: Option<DateTime<Utc>>) -> Result<Scan> {
        let since = since.and_then(|start| start.checked_sub_signed(chrono::Duration::days(2)));
        self.scan_modified_since(sources, since)
    }

    /// Read files at or after an exact modification-time cutoff, without extra grace.
    pub fn scan_modified_since(
        &mut self,
        sources: &[Source],
        since: Option<DateTime<Utc>>,
    ) -> Result<Scan> {
        cancel::check(&self.stopped)?;
        self.service_reconciliation(sources, since)?;
        let mut scan = Scan::default();
        let mut candidates = Vec::new();
        for source in sources {
            cancel::check(&self.stopped)?;
            match self.read_source(source, since) {
                Ok(snapshot) => {
                    let now = Utc::now();
                    scan.quota_snapshots
                        .extend(snapshot.quota_snapshots.clone());
                    for candidate in &snapshot.candidates {
                        cancel::check(&self.stopped)?;
                        candidates.push(candidate.clone());
                    }
                    add_coverage(&mut scan.coverage, &snapshot.coverage);
                    self.sources.insert(source.name.clone(), (snapshot, now));
                    scan.sources.push(SourceStatus {
                        name: source.name.clone(),
                        root: source.root.clone(),
                        readable: true,
                        last_success: Some(now),
                        error: None,
                    });
                }
                Err(error) => {
                    if cancel::is_cancelled(&error) {
                        return Err(error);
                    }
                    let previous = self.sources.get(&source.name);
                    if let Some((snapshot, _)) = previous {
                        scan.quota_snapshots
                            .extend(snapshot.quota_snapshots.clone());
                        for candidate in &snapshot.candidates {
                            cancel::check(&self.stopped)?;
                            candidates.push(candidate.clone());
                        }
                        add_coverage(&mut scan.coverage, &snapshot.coverage);
                    }
                    scan.sources.push(SourceStatus {
                        name: source.name.clone(),
                        root: source.root.clone(),
                        readable: false,
                        last_success: previous.map(|(_, time)| *time),
                        error: Some(error.to_string()),
                    });
                }
            }
        }
        for ((name, _), message) in &self.background_errors {
            cancel::check(&self.stopped)?;
            if sources.iter().any(|source| source.name == *name) {
                scan.coverage.uncertain += 1;
                note(&mut scan.coverage, message.clone());
            }
        }
        if self.reconcile_at.is_none() && self.sweep.is_empty() && self.reconcile_job.is_none() {
            self.reconcile_at = Some(Instant::now() + Duration::from_secs(60));
        }
        scan.events = reconcile(candidates, &mut scan.coverage, &self.stopped)?;
        scan.sources.sort_by(|a, b| a.name.cmp(&b.name));
        cancel::check(&self.stopped)?;
        Ok(scan)
    }

    fn service_reconciliation(
        &mut self,
        selected: &[Source],
        since: Option<DateTime<Utc>>,
    ) -> Result<()> {
        cancel::check(&self.stopped)?;
        if self
            .reconcile_job
            .as_ref()
            .is_some_and(|(_, job)| job.is_finished())
        {
            let (key, job) = self.reconcile_job.take().unwrap();
            match job.join() {
                Ok(Ok(parsed)) => {
                    let unchanged = fs::metadata(&key.1).ok().is_some_and(|metadata| {
                        metadata.len() == parsed.len
                            && metadata.modified().ok() == parsed.modified
                            && identity(&metadata) == parsed.identity
                    });
                    if unchanged && self.files.contains_key(&key) {
                        self.files.insert(key.clone(), parsed);
                        self.background_errors.remove(&key);
                    }
                }
                Ok(Err(error)) => {
                    if cancel::is_cancelled(&error) {
                        return Err(error);
                    }
                    self.background_errors.insert(key, error.to_string());
                }
                Err(_) => {
                    self.background_errors
                        .insert(key, "Background content reconciliation failed".into());
                }
            }
        }
        if self
            .reconcile_at
            .is_some_and(|deadline| Instant::now() >= deadline)
            && self.reconcile_job.is_none()
            && self.sweep.is_empty()
        {
            let mut keys: Vec<_> = self
                .files
                .keys()
                .filter(|(name, _)| selected.iter().any(|source| source.name == *name))
                .cloned()
                .collect();
            keys.sort();
            cancel::check(&self.stopped)?;
            self.sweep.extend(keys);
            self.reconcile_at = None;
        }
        // Recheck queued files as the monitor's date cutoff advances at midnight.
        let files = &self.files;
        self.sweep.retain(|key| {
            selected.iter().any(|source| source.name == key.0)
                && files
                    .get(key)
                    .is_some_and(|cached| eligible_modified(cached.modified, since))
        });
        // One background file at a time. Large archives never block the terminal event loop.
        if self.reconcile_job.is_none() {
            if let Some(key) = self.sweep.pop_front() {
                let path = key.1.clone();
                let stopped = Arc::clone(&self.stopped);
                let job = std::thread::spawn(move || {
                    cancel::check(&stopped)?;
                    let metadata = fs::metadata(&path)?;
                    parse_cached(&path, &metadata, None, &stopped)
                });
                self.reconcile_job = Some((key, job));
            }
        }
        cancel::check(&self.stopped)
    }

    fn read_source(
        &mut self,
        source: &Source,
        since: Option<DateTime<Utc>>,
    ) -> Result<SourceSnapshot> {
        let paths = discover(&source.root, &self.stopped)?;
        // Inspect first, then parse only changed files in parallel. Unchanged monitor
        // refreshes do not start workers, and each file retains its own ordered parser.
        let mut inspected = Vec::with_capacity(paths.len());
        let mut jobs = Vec::new();
        for path in paths {
            cancel::check(&self.stopped)?;
            let key = (source.name.clone(), path);
            let loaded = (|| -> Result<bool> {
                let metadata = fs::metadata(&key.1)
                    .with_context(|| format!("Cannot inspect {}", key.1.display()))?;
                if !eligible_modified(metadata.modified().ok(), since) {
                    return Ok(false);
                }
                let old = self.files.get(&key);
                if old.is_none_or(|cached| {
                    cached.len != metadata.len()
                        || cached.modified != metadata.modified().ok()
                        || cached.identity != identity(&metadata)
                }) {
                    jobs.push((inspected.len(), key.1.clone(), metadata, old));
                }
                Ok(true)
            })();
            inspected.push((key, loaded));
        }
        let parsed = parse_files(&jobs, &self.stopped)?;
        for (index, result) in parsed {
            match result {
                Ok(parsed) => {
                    let key = &inspected[index].0;
                    self.files.insert(key.clone(), parsed);
                    self.background_errors.remove(key);
                }
                Err(error) => inspected[index].1 = Err(error),
            }
        }
        let mut snapshot = SourceSnapshot::default();
        let mut present = BTreeSet::new();
        for (key, loaded) in inspected {
            cancel::check(&self.stopped)?;
            present.insert(key.clone());
            if matches!(loaded, Ok(false)) {
                present.remove(&key);
                continue;
            }
            if let Err(error) = loaded {
                if cancel::is_cancelled(&error) {
                    return Err(error);
                }
                snapshot.coverage.uncertain += 1;
                note(&mut snapshot.coverage, error.to_string());
                if !self.files.contains_key(&key) {
                    continue;
                }
            }
            let cached = self.files.get(&key).expect("file was parsed");
            snapshot.coverage.files += 1;
            for quota in &cached.parser.quota_snapshots {
                cancel::check(&self.stopped)?;
                let mut quota = quota.clone();
                quota.source.clone_from(&source.name);
                snapshot.quota_snapshots.push(quota);
            }
            add_coverage(&mut snapshot.coverage, &cached.parser.coverage);
            if !cached.parser.pending.is_empty() {
                snapshot.coverage.pending += 1;
            }
            let missing_compactions = cached
                .parser
                .compactions
                .difference(&cached.parser.direct_responses)
                .count();
            if missing_compactions > 0 {
                snapshot.coverage.uncertain += missing_compactions;
                note(
                    &mut snapshot.coverage,
                    "Compaction markers without recorded token usage",
                );
            }
            for candidate in &cached.parser.events {
                cancel::check(&self.stopped)?;
                let mut candidate = candidate.clone();
                if let Some(turn) = candidate
                    .turn
                    .as_ref()
                    .and_then(|id| cached.parser.turns.get(id))
                {
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
                candidate.event.source = source.name.clone();
                candidate.event.origins.insert(source.name.clone());
                snapshot.candidates.push(candidate);
            }
        }
        // A successful discovery removes contributions from deleted/moved/replaced files.
        self.files.retain(|(name, path), _| {
            name != &source.name || present.contains(&(name.clone(), path.clone()))
        });
        self.background_errors.retain(|(name, path), _| {
            name != &source.name || present.contains(&(name.clone(), path.clone()))
        });
        cancel::check(&self.stopped)?;
        Ok(snapshot)
    }
}

type ParseJob<'a> = (usize, PathBuf, Metadata, Option<&'a CachedFile>);
type ParsedFile = (usize, Result<CachedFile>);

fn parse_files(jobs: &[ParseJob<'_>], stopped: &Arc<AtomicBool>) -> Result<Vec<ParsedFile>> {
    // Bound both disk pressure and peak memory from large rollout records.
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8)
        .min(jobs.len());
    let parse = |(index, path, metadata, old): &ParseJob<'_>| {
        (*index, parse_cached(path, metadata, *old, stopped))
    };
    if workers <= 1 {
        return Ok(jobs.iter().map(parse).collect());
    }
    let next = AtomicUsize::new(0);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut results = Vec::new();
                    while !stopped.load(Ordering::Relaxed) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(index) else { break };
                        results.push(parse(job));
                    }
                    results
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("Session loading worker failed"))
            })
            .collect::<Result<Vec<_>>>()
    })?;
    cancel::check(stopped)?;
    Ok(results.into_iter().flatten().collect())
}

fn eligible_modified(modified: Option<SystemTime>, since: Option<DateTime<Utc>>) -> bool {
    match (modified, since) {
        (Some(modified), Some(since)) => DateTime::<Utc>::from(modified) >= since,
        _ => true,
    }
}

fn parse_cached(
    path: &Path,
    metadata: &Metadata,
    old: Option<&CachedFile>,
    stopped: &Arc<AtomicBool>,
) -> Result<CachedFile> {
    cancel::check(stopped)?;
    let identity = identity(metadata);
    let prefix = read_prefix(path, metadata.len().min(4096) as usize)?;
    cancel::check(stopped)?;
    let compressed = path.extension().is_some_and(|v| v == "zst");
    let append = !compressed
        && old.is_some_and(|cached| {
            metadata.len() > cached.len
                && cached.identity == identity
                && prefix.starts_with(&cached.prefix)
        });
    let mut parser = if append {
        old.unwrap().parser.clone()
    } else {
        Parser::default()
    };
    cancel::check(stopped)?;
    let file = File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    if compressed {
        let decoder = zstd::stream::read::Decoder::new(CancellableRead::new(file, stopped))
            .with_context(|| format!("Cannot decompress {}", path.display()))?;
        if let Err(error) = parser.consume(
            BufReader::new(CancellableRead::new(decoder, stopped)),
            path,
            stopped,
        ) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                parser.coverage.pending += 1;
            } else {
                return Err(error).with_context(|| format!("Cannot decompress {}", path.display()));
            }
        }
    } else {
        let mut file = file;
        if append {
            file.seek(SeekFrom::Start(parser.offset))?;
        }
        parser
            .consume(
                BufReader::new(CancellableRead::new(file, stopped)),
                path,
                stopped,
            )
            .with_context(|| format!("Cannot read {}", path.display()))?;
    }
    cancel::check(stopped)?;
    Ok(CachedFile {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        identity,
        prefix,
        parser,
    })
}

fn discover(root: &Path, stopped: &AtomicBool) -> Result<Vec<PathBuf>> {
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
    Ok(selected.into_values().map(|(_, path)| path).collect())
}

/// Check cancellation between buffered chunks, including giant newline-free records.
/// Interrupted errors would be retried by read_until, so use a typed Other error.
struct CancellableRead<'a, R> {
    inner: R,
    stopped: &'a AtomicBool,
}
impl<'a, R> CancellableRead<'a, R> {
    fn new(inner: R, stopped: &'a AtomicBool) -> Self {
        Self { inner, stopped }
    }
}
impl<R: Read> Read for CancellableRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other(cancel::Cancelled));
        }
        let read = self.inner.read(buf)?;
        if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other(cancel::Cancelled));
        }
        Ok(read)
    }
}
#[cfg(unix)]
fn identity(metadata: &Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.dev(), metadata.ino())
}
#[cfg(not(unix))]
fn identity(metadata: &Metadata) -> String {
    format!("{:?}", metadata.created().ok())
}
fn read_prefix(path: &Path, length: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0; length];
    File::open(path)?.read_exact(&mut buffer)?;
    Ok(buffer)
}
fn add_coverage(to: &mut Coverage, from: &Coverage) {
    to.files += from.files;
    to.malformed += from.malformed;
    to.pending += from.pending;
    to.duplicates += from.duplicates;
    to.conflicts += from.conflicts;
    to.uncertain += from.uncertain;
    for message in &from.messages {
        note(to, message);
    }
}
fn note(coverage: &mut Coverage, message: impl Into<String>) {
    if coverage.messages.len() < 32 {
        coverage.messages.insert(message.into());
    }
}
fn string(value: &Value, field: &str) -> Option<String> {
    value.get(field)?.as_str().map(str::to_owned)
}
fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("timestamp")?
        .as_str()?
        .parse::<DateTime<Utc>>()
        .ok()
}

impl Parser {
    fn consume(
        &mut self,
        mut reader: impl BufRead,
        path: &Path,
        stopped: &AtomicBool,
    ) -> std::io::Result<()> {
        loop {
            if stopped.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(std::io::Error::other(cancel::Cancelled));
            }
            let mut line = std::mem::take(&mut self.pending);
            let read = reader.read_until(b'\n', &mut line)?;
            if stopped.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(std::io::Error::other(cancel::Cancelled));
            }
            self.offset += read as u64;
            if read == 0 {
                self.pending = line;
                break;
            }
            if line.last() != Some(&b'\n') {
                if !line.iter().all(u8::is_ascii_whitespace) {
                    match serde_json::from_slice::<Value>(&line) {
                        Ok(_) => {
                            self.line += 1;
                            self.parse_line(&line, path);
                        }
                        Err(error) if error.is_eof() => self.pending = line,
                        Err(_) => {
                            self.line += 1;
                            self.problem(path, "Malformed final JSON record", true);
                        }
                    }
                }
                break;
            }
            self.line += 1;
            self.parse_line(&line, path);
        }
        Ok(())
    }
    fn problem(&mut self, path: &Path, message: &str, malformed: bool) {
        if malformed {
            self.coverage.malformed += 1;
        } else {
            self.coverage.uncertain += 1;
        }
        note(
            &mut self.coverage,
            format!("{}:{}: {message}", path.display(), self.line),
        );
    }
    fn parse_line(&mut self, bytes: &[u8], path: &Path) {
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
                    self.problem(path, "Unrecognized usage record type", false);
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
                self.problem(path, "Malformed JSON record", true);
                return;
            }
        };
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
                    self.owner_created = timestamp(payload).or_else(|| timestamp(&value));
                    self.parent = string(payload, "forked_from_id")
                        .or_else(|| string(payload, "parent_thread_id"));
                }
                if !session.is_empty() {
                    self.active_session = session;
                    self.current.tier = self
                        .thread_tiers
                        .get(&self.active_session)
                        .cloned()
                        .flatten();
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
            "token_usage_record" => self.direct(&value, path),
            "event_msg" if payload["type"] == "thread_settings_applied" => self.settings(payload),
            "event_msg" if payload["type"] == "token_count" => {
                if let Some(time) = timestamp(&value) {
                    self.quota_snapshots
                        .extend(crate::quota::QuotaSnapshot::parse(
                            time,
                            &payload["rate_limits"],
                            "",
                        ));
                }
                self.legacy(&value, path);
            }
            _ => {}
        }
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
        UsageEvent {
            id,
            timestamp: time,
            session,
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
            tier: recorded_tier(payload).unwrap_or_else(|| self.current.tier.clone()),
            project: self.current.project.clone(),
            usage,
            source: String::new(),
            origins: BTreeSet::new(),
            context_tokens: payload.get("context_tokens").and_then(Value::as_u64),
            cache_retention: string(payload, "cache_retention")
                .or_else(|| string(payload, "prompt_cache_retention")),
        }
    }
    fn direct(&mut self, value: &Value, path: &Path) {
        let payload = &value["payload"];
        let Some(time) = timestamp(value) else {
            self.problem(path, "Usage record has no valid timestamp", true);
            return;
        };
        let usage = match parse_usage(&payload["usage"]) {
            Ok(usage) => usage,
            Err(error) => {
                self.problem(path, &error, true);
                return;
            }
        };
        let session = string(payload, "thread_id")
            .or_else(|| string(payload, "session_id"))
            .unwrap_or_else(|| self.active_session.clone());
        if session.is_empty() {
            self.problem(path, "Usage record has no session identity", false);
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
            self.problem(
                path,
                "Per-response usage lacks stable response identity",
                false,
            );
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
            self.problem(path, "Standalone usage lacks turn/counter evidence for compaction reconciliation; excluded", false);
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
        self.recent_direct
            .insert(session, (event.usage.clone(), time, false));
        self.events.push(Candidate {
            event,
            file: path.to_path_buf(),
            turn,
            snapshot,
            direct: true,
        });
    }
    fn legacy(&mut self, value: &Value, path: &Path) {
        let payload = &value["payload"];
        let info = &payload["info"];
        if info.is_null() {
            return;
        }
        let Some(time) = timestamp(value) else {
            self.problem(path, "Token snapshot has no valid timestamp", true);
            return;
        };
        let total = match parse_usage(&info["total_token_usage"]) {
            Ok(total) => total,
            Err(error) => {
                self.problem(path, &error, true);
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
            self.problem(path, "Token snapshot has no session identity", false);
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
                    self.problem(path, "Cumulative counter decreased; baseline reset, ambiguous increment excluded", false);
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
            self.problem(path, &error.to_string(), true);
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
                path,
                "Cumulative delta spans a model change; model attribution is unknown",
                false,
            );
        }
        self.events.push(Candidate {
            event,
            file: path.to_path_buf(),
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
fn parse_usage(value: &Value) -> std::result::Result<Usage, String> {
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
fn same_event(a: &UsageEvent, b: &UsageEvent) -> bool {
    same_usage(a, b)
        && a.session == b.session
        && (a.model == "unknown" || b.model == "unknown" || a.model == b.model)
        && (a.provider == "unknown" || b.provider == "unknown" || a.provider == b.provider)
        && (a.tier.is_none()
            || b.tier.is_none()
            || a.tier == b.tier
            || (a.mode() != "unknown" && a.mode() == b.mode()))
        && compatible(a.context_tokens, b.context_tokens)
        && (a.cache_retention.is_none()
            || b.cache_retention.is_none()
            || a.cache_retention.as_deref().map(retention)
                == b.cache_retention.as_deref().map(retention))
}
fn retention(value: &str) -> &str {
    match value {
        "30min" => "30m",
        value => value,
    }
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
    if existing.context_tokens.is_none() {
        existing.context_tokens = other.context_tokens;
    }
    if existing.cache_retention.is_none() {
        existing.cache_retention.clone_from(&other.cache_retention);
    }
    if existing.project.is_none() {
        existing.project.clone_from(&other.project);
    }
}
fn merge_mirror(existing: &mut UsageEvent, mirror: &UsageEvent, coverage: &mut Coverage) {
    if same_event(existing, mirror) {
        merge_event(existing, mirror);
        coverage.duplicates += 1;
    } else {
        coverage.uncertain += 1;
        note(
            coverage,
            "Conflicting compatibility metadata excluded; identified per-response usage retained",
        );
    }
}
fn reconcile(
    mut candidates: Vec<Candidate>,
    coverage: &mut Coverage,
    stopped: &AtomicBool,
) -> Result<Vec<UsageEvent>> {
    cancel::check(stopped)?;
    // Prefer an identified response over its equivalent legacy snapshot from another copy.
    candidates.sort_by(|a, b| {
        b.direct
            .cmp(&a.direct)
            .then_with(|| a.event.source.cmp(&b.event.source))
            .then_with(|| a.event.id.cmp(&b.event.id))
    });
    cancel::check(stopped)?;
    let mut events: BTreeMap<String, UsageEvent> = BTreeMap::new();
    let mut direct_snapshots = HashMap::<_, BTreeSet<(String, DateTime<Utc>)>>::new();
    let mut direct_contexts = HashMap::<_, Vec<(String, PathBuf)>>::new();
    let mut conflicts = BTreeSet::new();
    let mut inconsistent_timestamps = BTreeSet::new();
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
                        merge_mirror(other, &event, coverage);
                    }
                    continue;
                } else if !endpoints.is_empty() {
                    coverage.uncertain += 1;
                    note(coverage, "Overlapping or ambiguous cumulative compatibility increment excluded; identified per-response usage retained");
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
                        merge_mirror(other, &event, coverage);
                    }
                    continue;
                } else if matching.len() > 1 {
                    coverage.uncertain += 1;
                    note(coverage, "Ambiguous copied compatibility record excluded: multiple direct responses match its session, turn, timestamp and usage");
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
                        coverage.uncertain += 1;
                        note(coverage, "Identified request copies have different timestamps; earliest recorded timestamp used for date attribution");
                    }
                    existing.timestamp = existing.timestamp.min(event.timestamp);
                }
                merge_event(existing, &event);
                coverage.duplicates += 1;
            } else {
                coverage.conflicts += 1;
                note(
                    coverage,
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
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::TempDir;

    fn source(root: &Path, name: &str) -> Source {
        Source {
            name: name.into(),
            kind: "linux".into(),
            root: root.into(),
            enabled: true,
        }
    }
    fn usage(input: u64, output: u64) -> Value {
        json!({"input_tokens":input,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":output,
            "reasoning_output_tokens":0,"total_tokens":input+output})
    }
    fn metadata(session: &str) -> Value {
        json!({"timestamp":"2026-11-01T10:00:00Z","type":"session_meta",
            "payload":{"id":session,"timestamp":"2026-11-01T10:00:00Z","cwd":"/project","model_provider":"openai"}})
    }
    fn turn(id: &str, model: &str) -> Value {
        json!({"timestamp":"2026-11-01T10:00:01Z","type":"turn_context","payload":{"turn_id":id,"model":model}})
    }
    fn direct(
        response: &str,
        session: &str,
        turn_id: &str,
        input: u64,
        output: u64,
        total_input: u64,
        total_output: u64,
    ) -> Value {
        json!({"timestamp":"2026-11-01T10:00:02Z","type":"token_usage_record", "payload":{
            "thread_id":session,"turn_id":turn_id,"response_id":response,"usage":usage(input,output),
            "thread_token_usage":usage(total_input,total_output)}})
    }
    fn legacy(input: u64, output: u64, seconds: u8) -> Value {
        json!({"timestamp":format!("2026-11-01T10:00:{seconds:02}Z"),"type":"event_msg", "payload":{
            "type":"token_count","info":{"total_token_usage":usage(input,output),"last_token_usage":usage(input,output)}}})
    }
    fn write(path: &Path, values: &[Value]) {
        let mut file = File::create(path).unwrap();
        for value in values {
            writeln!(file, "{value}").unwrap();
        }
    }
    fn modified_at(path: &Path, timestamp: DateTime<Utc>) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(SystemTime::from(timestamp)))
            .unwrap();
    }

    struct StopDuringRead<R> {
        inner: R,
        stopped: Arc<AtomicBool>,
        reads: usize,
        bytes: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl<R: Read> Read for StopDuringRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let count = self.inner.read(buf)?;
            self.bytes
                .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
            self.reads += 1;
            if self.reads == 2 {
                self.stopped
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(count)
        }
    }

    #[test]
    fn cancellation_interrupts_plain_and_compressed_giant_lines_between_chunks() {
        use std::io::Cursor;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let data = vec![b'x'; 1024 * 1024];
        for compressed in [false, true] {
            let stopped = Arc::new(AtomicBool::new(false));
            let bytes = Arc::new(AtomicUsize::new(0));
            let input: Box<dyn Read> = if compressed {
                let encoded = zstd::stream::encode_all(data.as_slice(), 1).unwrap();
                Box::new(zstd::stream::read::Decoder::new(Cursor::new(encoded)).unwrap())
            } else {
                Box::new(Cursor::new(data.clone()))
            };
            let trigger = StopDuringRead {
                inner: input,
                stopped: Arc::clone(&stopped),
                reads: 0,
                bytes: Arc::clone(&bytes),
            };
            let mut parser = Parser::default();
            let error = parser
                .consume(
                    BufReader::new(CancellableRead::new(trigger, &stopped)),
                    Path::new("fixture.jsonl"),
                    &stopped,
                )
                .unwrap_err();
            assert!(cancel::is_cancelled(&error.into()));
            assert!(
                bytes.load(Ordering::Relaxed) <= 16 * 1024,
                "did not stop between chunks"
            );
            assert!(parser.events.is_empty());
            assert_eq!(parser.coverage.malformed, 0);
            assert_eq!(parser.coverage.pending, 0);
        }
    }

    #[test]
    fn cancellation_is_propagated_before_discovery_and_reconciliation_and_can_retry() {
        use std::sync::atomic::Ordering;
        let stopped = Arc::new(AtomicBool::new(true));
        // Cancellation wins over a missing source: it must not become a stale-source snapshot.
        assert!(cancel::is_cancelled(
            &discover(Path::new("/missing-source"), &stopped).unwrap_err()
        ));
        assert!(cancel::is_cancelled(
            &reconcile(Vec::new(), &mut Coverage::default(), &stopped).unwrap_err()
        ));
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("usage.jsonl"),
            &[metadata("s"), direct("r", "s", "t", 10, 1, 10, 1)],
        );
        let sources = [source(dir.path(), "local")];
        let mut reader = Reader::with_cancel(Arc::clone(&stopped));
        assert!(cancel::is_cancelled(&reader.scan(&sources).unwrap_err()));
        assert!(reader.files.is_empty());
        assert!(reader.sources.is_empty());
        stopped.store(false, Ordering::Relaxed);
        let complete = reader.scan(&sources).unwrap();
        assert_eq!(complete.events.len(), 1);
        assert_eq!(complete.events[0].usage.total(), 11);
        stopped.store(true, Ordering::Relaxed);
        assert!(cancel::is_cancelled(&reader.scan(&sources).unwrap_err()));
        assert_eq!(reader.sources["local"].0.candidates.len(), 1);
        assert!(reader.background_errors.is_empty());
    }

    #[test]
    fn exact_monitor_cutoff_keeps_boundary_and_rechecks_resumed_old_files() {
        let dir = TempDir::new().unwrap();
        let cutoff: DateTime<Utc> = "2026-10-31T12:00:00Z".parse().unwrap();
        let boundary = dir.path().join("rollout-2026-01-01-boundary.jsonl");
        write(
            &boundary,
            &[metadata("s"), direct("r1", "s", "t", 10, 1, 10, 1)],
        );
        modified_at(&boundary, cutoff);
        let old = dir.path().join("old.jsonl.zst");
        fs::write(&old, b"invalid old archive must never be opened").unwrap();
        modified_at(&old, cutoff - chrono::Duration::seconds(1));
        let sources = [source(dir.path(), "local")];
        let mut reader = Reader::new();
        let first = reader.scan_modified_since(&sources, Some(cutoff)).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.coverage.files, 1);
        assert_eq!(first.coverage.uncertain, 0);
        let next = cutoff + chrono::Duration::days(1);
        reader.reconcile_at = Some(Instant::now());
        assert!(reader
            .scan_modified_since(&sources, Some(next))
            .unwrap()
            .events
            .is_empty());
        assert!(reader.reconcile_job.is_none());
        assert!(reader.files.is_empty());
        let mut file = File::options().append(true).open(&boundary).unwrap();
        writeln!(file, "{}", direct("r2", "s", "t", 20, 2, 30, 3)).unwrap();
        drop(file);
        modified_at(&boundary, next);
        let resumed = reader.scan_modified_since(&sources, Some(next)).unwrap();
        assert!(resumed.events.iter().any(|event| event.id == "response:r2"));
        assert_eq!(resumed.coverage.uncertain, 0);
    }

    #[test]
    fn early_filter_skips_old_archives_but_keeps_resumed_sessions_and_counter_baselines() {
        let dir = TempDir::new().unwrap();
        let start: DateTime<Utc> = "2026-11-01T10:00:10Z".parse().unwrap();
        let old = dir.path().join("old.jsonl");
        write(
            &old,
            &[metadata("old"), direct("old", "old", "t", 10, 1, 10, 1)],
        );
        modified_at(
            &old,
            start - chrono::Duration::days(2) - chrono::Duration::seconds(1),
        );
        let archive = dir.path().join("old-archive.jsonl.zst");
        fs::write(&archive, b"invalid archive must never be opened").unwrap();
        modified_at(
            &archive,
            start - chrono::Duration::days(2) - chrono::Duration::seconds(1),
        );
        let resumed = dir.path().join("rollout-2026-01-01-resumed.jsonl");
        write(
            &resumed,
            &[
                metadata("resumed"),
                turn("t", "test"),
                legacy(100, 10, 2),
                legacy(150, 20, 11),
            ],
        );
        // Exact boundary is eligible, independent of the session's creation date.
        modified_at(&resumed, start - chrono::Duration::days(2));
        let sources = [source(dir.path(), "local")];
        let mut filtered = Reader::new();
        let scan = filtered.scan_since(&sources, Some(start)).unwrap();
        assert_eq!(scan.coverage.files, 1);
        assert_eq!(scan.coverage.uncertain, 0);
        assert_eq!(filtered.files.len(), 1);
        let current: Vec<_> = scan
            .events
            .iter()
            .filter(|e| e.timestamp >= start)
            .collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].usage.total(), 60);
        let full = Reader::new().scan(&sources).unwrap();
        let expected: Vec<_> = full
            .events
            .iter()
            .filter(|e| e.timestamp >= start)
            .collect();
        assert_eq!(
            serde_json::to_value(current).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // --all still reads both eligible plain logs.
        assert_eq!(filtered.scan(&sources).unwrap().coverage.files, 2);
    }

    #[test]
    fn early_filter_rechecks_old_files_and_expires_cached_files_at_midnight() {
        let dir = TempDir::new().unwrap();
        let start: DateTime<Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let path = dir.path().join("resumed.jsonl");
        write(&path, &[metadata("s"), direct("r", "s", "t", 10, 1, 10, 1)]);
        modified_at(&path, start - chrono::Duration::days(2));
        let sources = [source(dir.path(), "local")];
        let mut reader = Reader::new();
        assert_eq!(
            reader
                .scan_since(&sources, Some(start))
                .unwrap()
                .events
                .len(),
            1
        );
        let tomorrow = start + chrono::Duration::days(1);
        assert!(reader
            .scan_since(&sources, Some(tomorrow))
            .unwrap()
            .events
            .is_empty());
        assert!(reader.files.is_empty());
        let mut event = direct("next", "s", "t", 20, 2, 30, 3);
        event["timestamp"] = json!(tomorrow.to_rfc3339());
        let mut file = File::options().append(true).open(&path).unwrap();
        writeln!(file, "{event}").unwrap();
        drop(file);
        modified_at(&path, tomorrow);
        let scan = reader.scan_since(&sources, Some(tomorrow)).unwrap();
        assert_eq!(
            scan.events
                .iter()
                .filter(|e| e.timestamp >= tomorrow)
                .count(),
            1
        );
    }
    #[test]
    fn current_model_changes_and_legacy_mirrors_count_once() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("rollout.jsonl"),
            &[
                metadata("s"),
                turn("t1", "alpha"),
                direct("r1", "s", "t1", 100, 20, 100, 20),
                legacy(100, 20, 2),
                turn("t2", "beta"),
                direct("r2", "s", "t2", 50, 5, 150, 25),
                legacy(150, 25, 3),
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events.iter().map(|e| e.usage.total()).sum::<u64>(),
            175
        );
        assert_eq!(
            scan.events
                .iter()
                .find(|e| e.id == "response:r2")
                .unwrap()
                .model,
            "beta"
        );
        assert!(!scan.coverage.incomplete(), "{:?}", scan.coverage);
    }

    #[test]
    fn cross_source_compatibility_mirrors_with_different_counter_bases_count_once() {
        let modern = TempDir::new().unwrap();
        let compatibility = TempDir::new().unwrap();
        let first = direct("r1", "s", "t1", 10, 1, 10, 1);
        let mut compaction = direct("compact", "s", "tc", 100, 10, 110, 11);
        compaction["timestamp"] = json!("2026-11-01T10:00:03Z");
        let mut second = direct("r2", "s", "t2", 20, 2, 130, 13);
        second["timestamp"] = json!("2026-11-01T10:00:04Z");
        write(
            &modern.path().join("modern.jsonl"),
            &[
                metadata("s"),
                turn("t1", "alpha"),
                first,
                turn("tc", "alpha"),
                compaction,
                turn("t2", "alpha"),
                second,
            ],
        );
        write(
            &compatibility.path().join("legacy.jsonl"),
            &[
                metadata("s"),
                turn("t1", "alpha"),
                legacy(10, 1, 2),
                turn("t2", "alpha"),
                legacy(30, 3, 4),
            ],
        );
        let scan = Reader::new()
            .scan(&[
                source(modern.path(), "wsl"),
                source(compatibility.path(), "windows"),
            ])
            .unwrap();
        assert_eq!(scan.events.len(), 3);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            143
        );
        // The same copied formats can live in separate files of one source.
        let combined = TempDir::new().unwrap();
        fs::copy(
            modern.path().join("modern.jsonl"),
            combined.path().join("modern.jsonl"),
        )
        .unwrap();
        fs::copy(
            compatibility.path().join("legacy.jsonl"),
            combined.path().join("legacy.jsonl"),
        )
        .unwrap();
        let scan = Reader::new()
            .scan(&[source(combined.path(), "local")])
            .unwrap();
        assert_eq!(scan.events.len(), 3);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            143
        );
    }
    #[test]
    fn different_counter_bases_require_exact_context_and_exclude_ambiguous_mirrors() {
        for variant in ["turn", "session", "timestamp", "missing-turn", "ambiguous"] {
            let dir = TempDir::new().unwrap();
            let mut values = vec![
                metadata("s"),
                turn("t", "alpha"),
                direct("r1", "s", "t", 20, 2, 120, 12),
            ];
            if variant == "ambiguous" {
                values.push(direct("r2", "s", "t", 20, 2, 140, 14));
            }
            write(&dir.path().join("modern.jsonl"), &values);
            let mut values = vec![metadata(if variant == "session" { "other" } else { "s" })];
            if variant != "missing-turn" {
                values.push(turn(if variant == "turn" { "other" } else { "t" }, "alpha"));
            }
            let mut mirror = legacy(20, 2, 2);
            if variant == "timestamp" {
                mirror["timestamp"] = json!("2026-11-01T10:00:02.001Z");
            }
            values.push(mirror);
            write(&dir.path().join("legacy.jsonl"), &values);
            let scan = Reader::new().scan(&[source(dir.path(), "local")]).unwrap();
            assert_eq!(scan.events.len(), 2, "{variant}");
            assert_eq!(
                scan.events
                    .iter()
                    .map(|event| event.usage.total())
                    .sum::<u64>(),
                44,
                "{variant}"
            );
            assert_eq!(
                scan.coverage.uncertain,
                usize::from(variant == "ambiguous"),
                "{variant}"
            );
        }
    }

    #[test]
    fn copied_request_aliases_and_missing_cost_metadata_merge_but_conflicts_do_not() {
        let dir = TempDir::new().unwrap();
        let mut first = direct("r", "s", "t", 100, 10, 100, 10);
        first["payload"]["service_tier"] = json!("fast");
        let mut second = first.clone();
        second["payload"]["service_tier"] = json!("priority");
        second["payload"]["context_tokens"] = json!(300000);
        second["payload"]["cache_retention"] = json!("12h");
        let path = dir.path().join("a.jsonl");
        write(&path, &[metadata("s"), first.clone()]);
        write(
            &dir.path().join("b.jsonl"),
            &[metadata("s"), second.clone()],
        );
        let sources = [source(dir.path(), "local")];
        let scan = Reader::new().scan(&sources).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].context_tokens, Some(300000));
        assert_eq!(scan.events[0].cache_retention.as_deref(), Some("12h"));
        assert_eq!(scan.coverage.conflicts, 0);
        first["payload"]["cache_retention"] = json!("30m");
        write(&path, &[metadata("s"), first.clone()]);
        let scan = Reader::new().scan(&sources).unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.coverage.conflicts, 1);
        first["payload"]["cache_retention"] = json!("12h");
        first["payload"]["context_tokens"] = json!(300001);
        write(&path, &[metadata("s"), first.clone()]);
        let scan = Reader::new().scan(&sources).unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.coverage.conflicts, 1);
        // Unknown service tiers must not compare equal merely because both display Unknown.
        first["payload"]["context_tokens"] = json!(300000);
        first["payload"]["service_tier"] = json!("unknown-one");
        second["payload"]["service_tier"] = json!("unknown-two");
        write(&path, &[metadata("s"), first]);
        write(&dir.path().join("b.jsonl"), &[metadata("s"), second]);
        let scan = Reader::new().scan(&sources).unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.coverage.conflicts, 1);
    }

    #[test]
    fn partial_legacy_aggregate_and_conflicting_mirror_do_not_add_duplicate_charges() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("modern.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                direct("r1", "s", "t", 100, 10, 100, 10),
                direct("r2", "s", "t", 20, 2, 120, 12),
            ],
        );
        let path = dir.path().join("legacy.jsonl");
        write(
            &path,
            &[metadata("s"), turn("t", "alpha"), legacy(120, 12, 2)],
        );
        let sources = [source(dir.path(), "local")];
        let scan = Reader::new().scan(&sources).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            132
        );
        assert_eq!(scan.coverage.uncertain, 1);
        let mut legacy_record = legacy(100, 10, 2);
        legacy_record["payload"]["info"]["service_tier"] = json!("priority");
        let mut context = turn("t", "alpha");
        context["payload"]["service_tier"] = json!("standard");
        write(
            &dir.path().join("modern.jsonl"),
            &[
                metadata("s"),
                context,
                direct("r1", "s", "t", 100, 10, 100, 10),
            ],
        );
        write(&path, &[metadata("s"), turn("t", "alpha"), legacy_record]);
        let scan = Reader::new().scan(&sources).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].usage.total(), 110);
        assert_eq!(scan.events[0].tier.as_deref(), Some("standard"));
        assert_eq!(scan.coverage.uncertain, 1);
    }

    #[test]
    fn copied_response_timestamp_disagreement_uses_earliest_date_with_warning() {
        use clap::Parser as _;
        let original = TempDir::new().unwrap();
        let copied = TempDir::new().unwrap();
        let old = direct("r", "s", "t", 10, 1, 10, 1);
        let mut retimestamped = old.clone();
        retimestamped["timestamp"] = json!("2026-11-04T10:00:02Z");
        write(
            &original.path().join("original.jsonl"),
            &[metadata("s"), old],
        );
        write(
            &copied.path().join("copy.jsonl"),
            &[metadata("s"), retimestamped],
        );
        for (old_name, copied_name) in [("a", "b"), ("b", "a")] {
            let scan = Reader::new()
                .scan(&[
                    source(original.path(), old_name),
                    source(copied.path(), copied_name),
                ])
                .unwrap();
            assert_eq!(scan.events.len(), 1);
            assert_eq!(
                scan.events[0].timestamp,
                "2026-11-01T10:00:02Z".parse::<DateTime<Utc>>().unwrap()
            );
            assert_eq!(scan.coverage.uncertain, 1);
            assert!(scan
                .coverage
                .messages
                .iter()
                .any(|message| message.contains("earliest recorded timestamp")));
            let today = crate::cli::Cli::parse_from(["codex-usage", "models", "--today"]);
            let all = crate::cli::Cli::parse_from(["codex-usage", "models", "--all"]);
            let now = "2026-11-04T12:00:00Z".parse().unwrap();
            assert_eq!(
                crate::report::build(&today, chrono_tz::UTC, &scan, None, now)
                    .unwrap()
                    .totals
                    .total,
                0
            );
            assert_eq!(
                crate::report::build(&all, chrono_tz::UTC, &scan, None, now)
                    .unwrap()
                    .totals
                    .total,
                11
            );
        }
    }

    #[test]
    fn every_identified_copy_indexes_its_cumulative_endpoint_without_duplicate_ids() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("a-modern.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                direct("r1", "s", "t", 100, 10, 100, 10),
                direct("r2", "s", "t", 20, 2, 120, 12),
            ],
        );
        // Another copy has a different cumulative base and was retimestamped days later.
        let mut later_copy = direct("r2", "s", "t", 20, 2, 220, 22);
        later_copy["timestamp"] = json!("2026-11-04T10:00:02Z");
        write(
            &dir.path().join("b-copy.jsonl"),
            &[metadata("s"), turn("t", "alpha"), later_copy],
        );
        let path = dir.path().join("c-legacy.jsonl");
        let mut aggregate = legacy(220, 22, 2);
        aggregate["timestamp"] = json!("2026-11-04T10:00:02Z");
        write(&path, &[metadata("s"), turn("t", "alpha"), aggregate]);
        let sources = [source(dir.path(), "local")];
        let scan = Reader::new().scan(&sources).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            132
        );
        assert_eq!(scan.coverage.uncertain, 2);
        // Repeated copies of one endpoint must still produce one unique response match.
        write(
            &dir.path().join("d-copy.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                direct("r2", "s", "t", 20, 2, 120, 12),
            ],
        );
        write(
            &path,
            &[
                metadata("s"),
                turn("t", "alpha"),
                legacy(100, 10, 2),
                legacy(120, 12, 3),
            ],
        );
        let scan = Reader::new().scan(&sources).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            132
        );
        assert_eq!(scan.coverage.uncertain, 1);
    }

    #[test]
    fn legacy_counters_reset_without_charging_full_snapshot() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("rollout.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                legacy(100, 10, 2),
                legacy(100, 10, 3),
                legacy(200, 20, 4),
                legacy(20, 2, 5),
                legacy(30, 3, 6),
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(
            scan.events.iter().map(|e| e.usage.total()).sum::<u64>(),
            231
        );
        assert_eq!(scan.coverage.uncertain, 1);
    }
    #[test]
    fn copies_and_forked_history_preserve_original_thread_identity() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        let values = [
            metadata("parent"),
            turn("p", "alpha"),
            direct("rp", "parent", "p", 100, 10, 100, 10),
        ];
        write(&left.path().join("parent.jsonl"), &values);
        let mut child_meta = metadata("child");
        child_meta["payload"]["parent_thread_id"] = json!("parent");
        child_meta["payload"]["forked_from_id"] = json!("parent");
        let mut child = vec![child_meta];
        child.extend(values.clone());
        child.extend([
            turn("c", "beta"),
            direct("rc", "child", "c", 50, 5, 50, 5),
            legacy(50, 5, 3),
        ]);
        write(&right.path().join("child.jsonl"), &child);
        write(&right.path().join("copy.jsonl"), &values);
        let scan = Reader::new()
            .scan(&[source(left.path(), "windows"), source(right.path(), "wsl")])
            .unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events.iter().map(|e| e.usage.total()).sum::<u64>(),
            165
        );
        let parent = scan.events.iter().find(|e| e.session == "parent").unwrap();
        assert_eq!(parent.origins.len(), 2);
        assert_eq!(scan.coverage.duplicates, 3);
    }
    #[test]
    fn append_partial_rewrite_move_and_source_recovery_reconcile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write(
            &path,
            &[
                metadata("s"),
                turn("t", "alpha"),
                direct("r1", "s", "t", 100, 10, 100, 10),
            ],
        );
        let sources = [source(dir.path(), "linux")];
        let mut reader = Reader::new();
        assert_eq!(reader.scan(&sources).unwrap().events.len(), 1);
        let record = direct("r2", "s", "t", 50, 5, 150, 15).to_string();
        let half = record.len() / 2;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&record.as_bytes()[..half]).unwrap();
        file.flush().unwrap();
        let pending = reader.scan(&sources).unwrap();
        assert_eq!(pending.events.len(), 1);
        assert_eq!(pending.coverage.pending, 1);
        file.write_all(&record.as_bytes()[half..]).unwrap();
        writeln!(file).unwrap();
        file.flush().unwrap();
        assert_eq!(reader.scan(&sources).unwrap().events.len(), 2);
        drop(file);
        write(
            &path,
            &[
                metadata("s"),
                turn("t", "beta"),
                direct("r3", "s", "t", 20, 2, 20, 2),
            ],
        );
        assert_eq!(reader.scan(&sources).unwrap().events[0].usage.total(), 22);
        let hidden = dir.path().with_extension("temporarily-offline");
        fs::rename(dir.path(), &hidden).unwrap();
        let stale = reader.scan(&sources).unwrap();
        assert_eq!(stale.events.len(), 1);
        assert!(!stale.sources[0].readable);
        fs::rename(&hidden, dir.path()).unwrap();
        fs::rename(&path, dir.path().join("moved.jsonl")).unwrap();
        let live = reader.scan(&sources).unwrap();
        assert_eq!(live.events.len(), 1);
        assert!(live.sources[0].readable);
    }

    #[test]
    fn complete_final_json_without_newline_counts_once() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.jsonl");
        write(&path, &[metadata("s"), turn("t", "alpha")]);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{}", direct("r1", "s", "t", 10, 1, 10, 1)).unwrap();
        file.flush().unwrap();
        let mut reader = Reader::new();
        let sources = [source(dir.path(), "linux")];
        let first = reader.scan(&sources).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.coverage.pending, 0);
        writeln!(file).unwrap();
        writeln!(file, "{}", direct("r2", "s", "t", 20, 2, 30, 3)).unwrap();
        file.flush().unwrap();
        let second = reader.scan(&sources).unwrap();
        assert_eq!(second.events.len(), 2);
        assert_eq!(second.coverage.pending, 0);
    }
    #[test]
    fn compressed_copy_plain_preference_and_unknown_model() {
        let dir = TempDir::new().unwrap();
        let active = dir.path().join("sessions");
        fs::create_dir(&active).unwrap();
        let archive = dir.path().join("archived_sessions");
        fs::create_dir(&archive).unwrap();
        let data = format!(
            "{}\n{}\n",
            metadata("s"),
            direct("r", "s", "t", 10, 1, 10, 1)
        );
        let compressed = zstd::encode_all(data.as_bytes(), 1).unwrap();
        fs::write(active.join("rollout.jsonl.zst"), compressed).unwrap();
        fs::write(
            archive.join("rollout.jsonl"),
            format!("{}\n", metadata("empty")),
        )
        .unwrap();
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.coverage.files, 1);
        assert_eq!(scan.events[0].model, "unknown");
    }
    #[test]
    fn malformed_counts_unknown_usage_and_missing_subsets_are_visible() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut record = direct("r", "s", "t", 10, 1, 10, 1);
        record["payload"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_write_input_tokens");
        write(
            &path,
            &[
                metadata("s"),
                record,
                json!({"type":"new_token_usage","payload":{}}),
            ],
        );
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "not json"
        )
        .unwrap();
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events[0].usage.cache_write, None);
        assert_eq!(scan.coverage.uncertain, 1);
        assert_eq!(scan.coverage.malformed, 1);
    }
    #[test]
    fn conflicting_response_is_excluded_instead_of_guessed() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("a.jsonl"),
            &[metadata("s"), direct("r", "s", "t", 10, 1, 10, 1)],
        );
        write(
            &dir.path().join("b.jsonl"),
            &[metadata("s"), direct("r", "s", "t", 20, 2, 20, 2)],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert!(scan.events.is_empty());
        assert_eq!(scan.coverage.conflicts, 1);
    }

    #[test]
    fn equal_independent_requests_without_response_ids_are_not_collapsed() {
        let dir = TempDir::new().unwrap();
        let mut record = direct("unused", "s", "t", 10, 1, 10, 1);
        record["payload"]
            .as_object_mut()
            .unwrap()
            .remove("response_id");
        record["payload"]
            .as_object_mut()
            .unwrap()
            .remove("thread_token_usage");
        write(
            &dir.path().join("a.jsonl"),
            &[metadata("s"), record.clone(), record],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            22
        );
        assert_eq!(scan.coverage.uncertain, 2);
    }

    #[test]
    fn settings_and_reused_turn_contexts_do_not_reprice_earlier_responses() {
        let dir = TempDir::new().unwrap();
        let setting =
            json!({"type":"thread_settings_applied","payload":{"service_tier":"priority"}});
        write(
            &dir.path().join("a.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                direct("r1", "s", "t", 10, 1, 10, 1),
                setting,
                turn("t", "beta"),
                direct("r2", "s", "t", 20, 2, 30, 3),
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        let first = scan
            .events
            .iter()
            .find(|event| event.id == "response:r1")
            .unwrap();
        let second = scan
            .events
            .iter()
            .find(|event| event.id == "response:r2")
            .unwrap();
        assert_eq!(first.model, "alpha");
        assert_eq!(first.tier, None);
        assert_eq!(second.model, "beta");
        assert_eq!(second.tier.as_deref(), Some("priority"));
    }

    #[test]
    fn recorded_thread_settings_and_request_tiers_keep_switch_chronology() {
        let dir = TempDir::new().unwrap();
        let setting = |tier| {
            json!({"type":"event_msg","payload":{
            "type":"thread_settings_applied","thread_id":"s","thread_settings":{"service_tier":tier}}})
        };
        let mut regular = turn("t", "alpha");
        regular["payload"]["service_tier"] = json!("standard");
        let mut request_override = direct("r4", "s", "t", 10, 1, 40, 4);
        request_override["payload"]["service_tier"] = json!("priority");
        write(
            &dir.path().join("a.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                setting(json!("priority")),
                direct("r1", "s", "t", 10, 1, 10, 1),
                regular,
                direct("r2", "s", "t", 10, 1, 20, 2),
                setting(Value::Null),
                direct("r3", "s", "t", 10, 1, 30, 3),
                request_override,
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        let tiers: Vec<_> = ["r1", "r2", "r3", "r4"]
            .iter()
            .map(|id| {
                scan.events
                    .iter()
                    .find(|event| event.id == format!("response:{id}"))
                    .unwrap()
                    .tier
                    .as_deref()
            })
            .collect();
        assert_eq!(
            tiers,
            vec![Some("priority"), Some("standard"), None, Some("priority")]
        );
    }

    #[test]
    fn inherited_parent_settings_do_not_classify_unrecorded_child_mode() {
        let dir = TempDir::new().unwrap();
        let mut child = metadata("child");
        child["payload"]["parent_thread_id"] = json!("parent");
        let setting = json!({"type":"event_msg","payload":{"type":"thread_settings_applied",
            "thread_id":"parent","thread_settings":{"service_tier":"priority"}}});
        write(
            &dir.path().join("a.jsonl"),
            &[
                child,
                metadata("parent"),
                turn("parent-turn", "alpha"),
                setting,
                direct("inherited", "parent", "parent-turn", 10, 1, 10, 1),
                turn("child-turn", "alpha"),
                direct("own", "child", "child-turn", 20, 2, 20, 2),
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tier, None);
    }

    #[test]
    fn inherited_turn_tier_is_scoped_to_parent_thread() {
        let dir = TempDir::new().unwrap();
        let created: DateTime<Utc> = "2026-11-01T10:00:00Z".parse().unwrap();
        let mut child = metadata("child");
        child["payload"]["parent_thread_id"] = json!("parent");
        let old_turn = uuid_at(created.timestamp_millis() - 500, 1);
        let own_turn = uuid_at(created.timestamp_millis() + 500, 2);
        let mut parent_context = turn(&old_turn, "alpha");
        parent_context["payload"]["service_tier"] = json!("priority");
        write(
            &dir.path().join("a.jsonl"),
            &[
                child,
                parent_context,
                direct("inherited", "parent", &old_turn, 10, 1, 10, 1),
                turn(&own_turn, "alpha"),
                direct("own", "child", &own_turn, 20, 2, 20, 2),
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tier, None);
    }

    #[test]
    fn duplicate_copy_can_supply_recorded_mode_missing_in_other_copy() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        let original = direct("r", "s", "t", 10, 1, 10, 1);
        let mut enriched = original.clone();
        enriched["payload"]["service_tier"] = json!("priority");
        write(&left.path().join("a.jsonl"), &[metadata("s"), original]);
        write(&right.path().join("b.jsonl"), &[metadata("s"), enriched]);
        let scan = Reader::new()
            .scan(&[source(left.path(), "a"), source(right.path(), "b")])
            .unwrap();
        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tier.as_deref(), Some("priority"));
        assert_eq!(scan.events[0].origins.len(), 2);
    }

    #[test]
    fn ambiguous_model_boundary_and_standalone_compaction_are_disclosed() {
        let dir = TempDir::new().unwrap();
        let mut later = legacy(200, 20, 3);
        later["payload"]["info"]["last_token_usage"] = usage(50, 5);
        let standalone = json!({"timestamp":"2026-11-01T10:00:04Z","type":"token_usage_record",
            "payload":{"response_id":"compaction","usage":usage(10,1)}});
        write(
            &dir.path().join("a.jsonl"),
            &[
                metadata("s"),
                turn("a", "alpha"),
                legacy(100, 10, 2),
                turn("b", "beta"),
                later,
                standalone,
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events
                .iter()
                .filter(|event| event.model == "unknown")
                .count(),
            1
        );
        assert_eq!(scan.coverage.uncertain, 2);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            220
        );
    }

    #[test]
    fn current_and_compatibility_counters_may_have_different_bases() {
        let dir = TempDir::new().unwrap();
        let mut first = legacy(10_000, 1_000, 2);
        first["payload"]["info"]["last_token_usage"] = usage(10, 1);
        let mut second = legacy(10_020, 1_002, 3);
        second["payload"]["info"]["last_token_usage"] = usage(20, 2);
        write(
            &dir.path().join("a.jsonl"),
            &[
                metadata("s"),
                turn("t", "alpha"),
                direct("r1", "s", "t", 10, 1, 10, 1),
                first,
                direct("r2", "s", "t", 20, 2, 30, 3),
                second,
            ],
        );
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 2);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            33
        );
        assert_eq!(scan.coverage.uncertain, 0);
    }

    #[test]
    fn bad_leaf_does_not_discard_readable_source_files() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("good.jsonl"),
            &[metadata("s"), direct("r", "s", "t", 10, 1, 10, 1)],
        );
        fs::write(dir.path().join("bad.jsonl.zst"), b"not zstandard data").unwrap();
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 1);
        assert!(scan.sources[0].readable);
        assert_eq!(scan.coverage.uncertain, 1);
    }

    #[test]
    fn parallel_loading_matches_serial_for_mixed_files_and_incremental_updates() {
        let dir = TempDir::new().unwrap();
        for index in 0..24 {
            let records = [
                metadata(&format!("s{index}")),
                turn("t", "alpha"),
                direct(
                    &format!("r{index}"),
                    &format!("s{index}"),
                    "t",
                    10,
                    1,
                    10,
                    1,
                ),
            ];
            if index % 2 == 0 {
                write(&dir.path().join(format!("{index:02}.jsonl")), &records);
            } else {
                let data = records.iter().map(|v| format!("{v}\n")).collect::<String>();
                fs::write(
                    dir.path().join(format!("{index:02}.jsonl.zst")),
                    zstd::encode_all(data.as_bytes(), 1).unwrap(),
                )
                .unwrap();
            }
        }
        fs::copy(dir.path().join("00.jsonl"), dir.path().join("copy.jsonl")).unwrap();
        fs::write(dir.path().join("bad.jsonl.zst"), b"invalid archive").unwrap();
        let sources = [source(dir.path(), "local")];
        let mut parallel = Reader::new();
        for updated in [false, true] {
            if updated {
                for index in (0..24).step_by(2) {
                    let mut file = fs::OpenOptions::new()
                        .append(true)
                        .open(dir.path().join(format!("{index:02}.jsonl")))
                        .unwrap();
                    writeln!(
                        file,
                        "{}",
                        direct(
                            &format!("next{index}"),
                            &format!("s{index}"),
                            "t",
                            20,
                            2,
                            30,
                            3
                        )
                    )
                    .unwrap();
                    write!(file, "{{\"type\":").unwrap();
                }
                fs::remove_file(dir.path().join("01.jsonl.zst")).unwrap();
            }
            // Seed the reference cache one file at a time, bypassing the worker loader.
            let mut serial = Reader::new();
            for path in discover(dir.path(), &serial.stopped).unwrap() {
                let metadata = fs::metadata(&path).unwrap();
                if let Ok(parsed) = parse_cached(&path, &metadata, None, &serial.stopped) {
                    serial.files.insert(("local".into(), path), parsed);
                }
            }
            let expected = serial.scan(&sources).unwrap();
            let actual = parallel.scan(&sources).unwrap();
            assert_eq!(actual.events, expected.events);
            assert_eq!(actual.events.len(), if updated { 35 } else { 24 });
            assert_eq!(
                serde_json::to_value(&actual.coverage).unwrap(),
                serde_json::to_value(&expected.coverage).unwrap()
            );
            assert_eq!(actual.coverage.pending, if updated { 12 } else { 0 });
            assert_eq!(actual.coverage.uncertain, 1);
        }
    }

    #[test]
    fn periodic_reconciliation_starts_after_initial_scan_and_runs_in_background() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.jsonl");
        let values = [metadata("s"), direct("r", "s", "t", 10, 1, 10, 1)];
        write(&path, &values);
        let sources = [source(dir.path(), "linux")];
        let mut reader = Reader::new();
        reader.scan(&sources).unwrap();
        assert!(reader.reconcile_at.unwrap() > Instant::now());
        assert!(reader.reconcile_job.is_none());
        let original_time = fs::metadata(&path).unwrap().modified().unwrap();
        let original_size = fs::metadata(&path).unwrap().len();
        write(&path, &[metadata("s"), direct("r", "s", "t", 20, 2, 20, 2)]);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(original_time))
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), original_size);
        // Force unchanged fingerprints independently of filesystem timestamp precision
        // or Windows creation-time identity changes across a UNC-backed rewrite.
        let rewritten = fs::metadata(&path).unwrap();
        let cached = reader
            .files
            .get_mut(&("linux".into(), path.clone()))
            .unwrap();
        cached.modified = rewritten.modified().ok();
        cached.identity = identity(&rewritten);
        assert_eq!(reader.scan(&sources).unwrap().events[0].usage.total(), 11);
        reader.reconcile_at = Some(Instant::now() - Duration::from_secs(1));
        reader.scan(&sources).unwrap();
        assert!(reader.reconcile_job.is_some());
        let deadline = Instant::now() + Duration::from_secs(2);
        while reader.reconcile_job.is_some() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
            reader.scan(&sources).unwrap();
        }
        assert_eq!(reader.scan(&sources).unwrap().events[0].usage.total(), 22);
        assert!(reader.reconcile_at.unwrap() > Instant::now());
        assert!(reader.reconcile_job.is_none());
    }

    fn uuid_at(millis: i64, suffix: u64) -> String {
        format!(
            "{:08x}-{:04x}-7000-8000-{suffix:012x}",
            millis >> 16,
            millis & 0xffff
        )
    }

    #[test]
    fn retimestamped_fork_prefix_is_a_baseline_not_child_usage() {
        let dir = TempDir::new().unwrap();
        let created: DateTime<Utc> = "2026-11-01T10:00:00Z".parse().unwrap();
        let parent = uuid_at(created.timestamp_millis() - 1000, 1);
        let inherited_turn = uuid_at(created.timestamp_millis() - 500, 2);
        write(
            &dir.path().join("parent.jsonl"),
            &[
                metadata(&parent),
                turn(&inherited_turn, "alpha"),
                legacy(100, 10, 2),
                legacy(200, 20, 3),
            ],
        );
        for (index, input, output) in [(1, 250, 25), (2, 270, 27)] {
            let child = uuid_at(created.timestamp_millis(), 10 + index);
            let mut meta = metadata(&child);
            meta["payload"]["parent_thread_id"] = json!(parent);
            meta["payload"]["forked_from_id"] = json!(parent);
            write(
                &dir.path().join(format!("child-{index}.jsonl")),
                &[
                    meta,
                    turn(&inherited_turn, "alpha"),
                    legacy(100, 10, 2),
                    legacy(200, 20, 3),
                    turn(
                        &uuid_at(created.timestamp_millis() + 500, 100 + index),
                        "beta",
                    ),
                    legacy(input, output, 4),
                ],
            );
        }
        let scan = Reader::new().scan(&[source(dir.path(), "linux")]).unwrap();
        assert_eq!(scan.events.len(), 4);
        assert_eq!(
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            352
        );
    }

    #[test]
    #[ignore = "read-only local schema smoke: set CODEX_USAGE_TEST_ROLLOUT"]
    fn local_rollout_read_only_smoke() {
        let path =
            PathBuf::from(std::env::var_os("CODEX_USAGE_TEST_ROLLOUT").expect("set rollout path"));
        let mut parser = Parser::default();
        parser
            .consume(
                BufReader::new(File::open(&path).unwrap()),
                &path,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            parser.coverage.malformed, 0,
            "{:?}",
            parser.coverage.messages
        );
        let mut coverage = Coverage::default();
        let events = reconcile(parser.events, &mut coverage, &AtomicBool::new(false)).unwrap();
        assert!(!events.is_empty());
        assert_eq!(coverage.conflicts, 0);
        eprintln!(
            "Read-only local smoke: {} usage increments, {} total tokens; {} uncertain",
            events.len(),
            events.iter().map(|event| event.usage.total()).sum::<u64>(),
            parser.coverage.uncertain
        );
    }
}
