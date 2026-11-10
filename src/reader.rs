//! Read-only transcript discovery, incremental JSONL framing, and adaptation.
//!
//! File lifecycle and framing are shared. Parsing and reconciliation belong to
//! the selected agent adapter; the reader only orchestrates and merges.
use crate::{
    agents::{
        self, add_coverage, note, AgentScan, DiscoveredFile, ParsedFileSnapshot, ParserState,
        RecordLocation,
    },
    cancel,
    model::{AgentKind, Coverage, QuotaCapability, Scan, Source, SourceKey, SourceStatus},
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

type FileKey = (SourceKey, PathBuf);

#[derive(Clone, Default)]
pub struct JsonlCursor {
    pub offset: u64,
    pub line: u64,
    pub pending: Vec<u8>,
}
pub enum Frame<'a> {
    Line(&'a [u8]),
    MalformedFinal(&'a [u8]),
}
impl JsonlCursor {
    /// Consume complete JSONL records. Complete JSON at EOF is consumed without a
    /// trailing newline; an incomplete final record is retained for the next append.
    pub fn consume(
        &mut self,
        mut reader: impl BufRead,
        stopped: &AtomicBool,
        mut on_line: impl FnMut(Frame<'_>, u64),
    ) -> std::io::Result<()> {
        loop {
            if stopped.load(Ordering::Relaxed) {
                return Err(std::io::Error::other(cancel::Cancelled));
            }
            let mut line = std::mem::take(&mut self.pending);
            let read = reader.read_until(b'\n', &mut line)?;
            if stopped.load(Ordering::Relaxed) {
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
                            let number = self.line;
                            on_line(Frame::Line(&line), number);
                        }
                        Err(error) if error.is_eof() => self.pending = line,
                        Err(_) => {
                            self.line += 1;
                            let number = self.line;
                            on_line(Frame::MalformedFinal(&line), number);
                        }
                    }
                }
                break;
            }
            self.line += 1;
            let number = self.line;
            on_line(Frame::Line(&line), number);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct CachedFile {
    file: DiscoveredFile,
    len: u64,
    modified: Option<SystemTime>,
    identity: String,
    prefix: Vec<u8>,
    cursor: JsonlCursor,
    parser: ParserState,
}
impl CachedFile {
    fn agent(&self) -> AgentKind {
        self.parser.agent()
    }
}
#[derive(Clone, Default)]
struct SourceSnapshot {
    files: Vec<ParsedFileSnapshot>,
    quota_snapshots: Vec<crate::quota::QuotaSnapshot>,
    coverage: Coverage,
}
type ReconcileJob = (FileKey, std::thread::JoinHandle<Result<CachedFile>>);

#[derive(Default)]
pub struct Reader {
    stopped: Arc<AtomicBool>,
    files: HashMap<FileKey, CachedFile>,
    sources: HashMap<SourceKey, (SourceSnapshot, DateTime<Utc>)>,
    reconcile_at: Option<Instant>,
    sweep: VecDeque<FileKey>,
    reconcile_job: Option<ReconcileJob>,
    background_errors: BTreeMap<FileKey, String>,
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
        let mut snapshots: Vec<(&Source, SourceSnapshot)> = Vec::new();
        let mut status: Vec<SourceStatus> = Vec::new();
        let mut file_coverage: BTreeMap<String, Coverage> = BTreeMap::new();
        for source in sources {
            cancel::check(&self.stopped)?;
            let key = SourceKey::new(source);
            match self.read_source(source, since) {
                Ok(snapshot) => {
                    let now = Utc::now();
                    add_coverage(&mut scan.coverage, &snapshot.coverage);
                    scan.quota_snapshots
                        .extend(snapshot.quota_snapshots.clone());
                    file_coverage.insert(source.name.clone(), snapshot.coverage.clone());
                    self.sources.insert(key.clone(), (snapshot.clone(), now));
                    snapshots.push((source, snapshot));
                    status.push(SourceStatus {
                        name: source.name.clone(),
                        agent: source.agent,
                        root: source.root.clone(),
                        readable: true,
                        last_success: Some(now),
                        error: None,
                        capability: capability(source.agent),
                        coverage: Coverage::default(),
                        pricing_provider: source.pricing_provider.clone(),
                    });
                }
                Err(error) => {
                    if cancel::is_cancelled(&error) {
                        return Err(error);
                    }
                    let previous = self.sources.get(&key).cloned();
                    if let Some((snapshot, _)) = &previous {
                        add_coverage(&mut scan.coverage, &snapshot.coverage);
                        scan.quota_snapshots
                            .extend(snapshot.quota_snapshots.clone());
                        file_coverage.insert(source.name.clone(), snapshot.coverage.clone());
                        snapshots.push((source, snapshot.clone()));
                    }
                    status.push(SourceStatus {
                        name: source.name.clone(),
                        agent: source.agent,
                        root: source.root.clone(),
                        readable: false,
                        last_success: previous.as_ref().map(|(_, time)| *time),
                        error: Some(error.to_string()),
                        capability: capability(source.agent),
                        coverage: Coverage::default(),
                        pricing_provider: source.pricing_provider.clone(),
                    });
                }
            }
        }
        for (key, message) in &self.background_errors {
            cancel::check(&self.stopped)?;
            if sources.iter().any(|source| source.name == key.0.name) {
                if let Some(coverage) = file_coverage.get_mut(&key.0.name) {
                    coverage.uncertain += 1;
                    note(coverage, message.clone());
                }
                scan.coverage.uncertain += 1;
                note(&mut scan.coverage, message.clone());
            }
        }
        if self.reconcile_at.is_none() && self.sweep.is_empty() && self.reconcile_job.is_none() {
            self.reconcile_at = Some(Instant::now() + Duration::from_secs(60));
        }
        // Reconcile each agent's files across all selected sources.
        let agents: BTreeSet<AgentKind> = sources.iter().map(|source| source.agent).collect();
        for agent in agents {
            let files: Vec<ParsedFileSnapshot> = snapshots
                .iter()
                .filter(|(source, _)| source.agent == agent)
                .flat_map(|(_, snapshot)| snapshot.files.iter().cloned())
                .collect();
            if files.is_empty() {
                continue;
            }
            let AgentScan {
                events,
                observed_event_ids,
                sessions,
                coverage,
                source_coverage,
            } = agents::reconcile(agent, &files, &self.stopped)?;
            add_coverage(&mut scan.coverage, &coverage);
            scan.events.extend(events);
            scan.observed_event_ids.extend(observed_event_ids);
            for (key, info) in sessions {
                scan.sessions
                    .entry(key.clone())
                    .or_insert_with(|| crate::model::SessionInfo::new(key))
                    .merge(&info);
            }
            for (name, coverage) in source_coverage {
                if let Some(file) = file_coverage.get_mut(&name) {
                    add_coverage(file, &coverage);
                }
            }
        }
        for source in &mut status {
            if let Some(coverage) = file_coverage.get(&source.name) {
                source.coverage.clone_from(coverage);
            }
        }
        scan.events
            .sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
        scan.sources = status;
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
                .filter(|(key, _)| selected.iter().any(|source| SourceKey::new(source) == *key))
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
            selected
                .iter()
                .any(|source| SourceKey::new(source) == key.0)
                && files
                    .get(key)
                    .is_some_and(|cached| eligible_modified(cached.modified, since))
        });
        // One background file at a time. Large archives never block the terminal event loop.
        if self.reconcile_job.is_none() {
            if let Some(key) = self.sweep.pop_front() {
                let path = key.1.clone();
                let source = selected
                    .iter()
                    .find(|source| SourceKey::new(source) == key.0)
                    .cloned();
                let file = self.files[&key].file.clone();
                let stopped = Arc::clone(&self.stopped);
                let job = std::thread::spawn(move || {
                    cancel::check(&stopped)?;
                    let metadata = fs::metadata(&path)?;
                    let source = source.context("Reconciliation source disappeared")?;
                    parse_cached(&path, &metadata, None, &source, &file, &stopped)
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
        let discovered = agents::discover(source, &self.stopped)?;
        // Inspect first, then parse only changed files in parallel. Unchanged monitor
        // refreshes do not start workers, and each file retains its own ordered parser.
        let mut inspected = Vec::with_capacity(discovered.len());
        let mut jobs = Vec::new();
        let source_key = SourceKey::new(source);
        for file in discovered {
            cancel::check(&self.stopped)?;
            let key = (source_key.clone(), file.path.clone());
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
                        || cached.agent() != source.agent
                }) {
                    jobs.push((inspected.len(), file.clone(), metadata, old.cloned()));
                }
                Ok(true)
            })();
            inspected.push((key, file, loaded));
        }
        let parsed = parse_files(&jobs, source, &self.stopped)?;
        for (index, result) in parsed {
            match result {
                Ok(parsed) => {
                    let key = &inspected[index].0;
                    self.files.insert(key.clone(), parsed);
                    self.background_errors.remove(key);
                }
                Err(error) => inspected[index].2 = Err(error),
            }
        }
        let mut snapshot = SourceSnapshot::default();
        let mut present = BTreeSet::new();
        for (key, _file, loaded) in inspected {
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
            snapshot
                .quota_snapshots
                .extend(cached.parser.quota_snapshots().iter().cloned());
            add_coverage(&mut snapshot.coverage, cached.parser.coverage());
            if !cached.cursor.pending.is_empty() {
                snapshot.coverage.pending += 1;
            }
            let missing_compactions = cached.parser.missing_compactions();
            if missing_compactions > 0 {
                snapshot.coverage.uncertain += missing_compactions;
                note(
                    &mut snapshot.coverage,
                    "Compaction markers without recorded token usage",
                );
            }
            snapshot.files.push(cached.parser.snapshot());
        }
        // A successful discovery removes contributions from deleted/moved/replaced files.
        self.files.retain(|(source_key, path), _| {
            *source_key != SourceKey::new(source)
                || present.contains(&(source_key.clone(), path.clone()))
        });
        self.background_errors.retain(|(source_key, path), _| {
            *source_key != SourceKey::new(source)
                || present.contains(&(source_key.clone(), path.clone()))
        });
        cancel::check(&self.stopped)?;
        Ok(snapshot)
    }
}

fn capability(agent: AgentKind) -> QuotaCapability {
    match agent {
        AgentKind::Codex => QuotaCapability::RecordedSnapshots,
        AgentKind::ClaudeCode => QuotaCapability::Unsupported,
    }
}

type ParseJob<'a> = (usize, DiscoveredFile, Metadata, Option<CachedFile>);
type ParsedFile = (usize, Result<CachedFile>);

fn parse_files(
    jobs: &[ParseJob<'_>],
    source: &Source,
    stopped: &Arc<AtomicBool>,
) -> Result<Vec<ParsedFile>> {
    // Bound both disk pressure and peak memory from large transcript records.
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8)
        .min(jobs.len());
    let parse = |(index, file, metadata, old): &ParseJob<'_>| {
        (
            *index,
            parse_cached(&file.path, metadata, old.as_ref(), source, file, stopped),
        )
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
    source: &Source,
    file: &DiscoveredFile,
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
                && cached.agent() == source.agent
        });
    let (mut cursor, mut parser) = match (append, old) {
        (true, Some(cached)) => (cached.cursor.clone(), cached.parser.clone()),
        _ => (
            JsonlCursor::default(),
            ParserState::new(source.agent, &source.name, file),
        ),
    };
    cancel::check(stopped)?;
    let mut file_handle =
        File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    if append && !compressed {
        file_handle.seek(SeekFrom::Start(cursor.offset))?;
    }
    let mut consume = |reader: &mut dyn BufRead| -> std::io::Result<()> {
        cursor.consume(reader, stopped, |frame, line| {
            let at = RecordLocation { path, line };
            match frame {
                Frame::Line(bytes) => parser.ingest(bytes, at),
                Frame::MalformedFinal(_) => parser.malformed_final(at),
            }
        })
    };
    if compressed {
        let decoder = zstd::stream::read::Decoder::new(CancellableRead::new(file_handle, stopped))
            .with_context(|| format!("Cannot decompress {}", path.display()))?;
        let mut reader = BufReader::new(CancellableRead::new(decoder, stopped));
        if let Err(error) = consume(&mut reader) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                parser.add_pending();
            } else {
                return Err(error).with_context(|| format!("Cannot decompress {}", path.display()));
            }
        }
    } else {
        let mut reader = BufReader::new(CancellableRead::new(file_handle, stopped));
        consume(&mut reader).with_context(|| format!("Cannot read {}", path.display()))?;
    }
    cancel::check(stopped)?;
    Ok(CachedFile {
        file: file.clone(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
        identity,
        prefix,
        cursor,
        parser,
    })
}

/// Legacy Codex discovery entry point retained for read-only local smoke tests.
pub fn discover(root: &Path, stopped: &AtomicBool) -> Result<Vec<PathBuf>> {
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
        if self.stopped.load(Ordering::Relaxed) {
            return Err(std::io::Error::other(cancel::Cancelled));
        }
        let read = self.inner.read(buf)?;
        if self.stopped.load(Ordering::Relaxed) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheWriteBreakdown, SessionKey, UsageEvent};
    use serde_json::{json, Value};
    use std::io::Write;
    use tempfile::TempDir;

    fn source(root: &Path, name: &str) -> Source {
        Source {
            name: name.into(),
            agent: AgentKind::Codex,
            kind: "linux".into(),
            root: root.into(),
            enabled: true,
            pricing_provider: None,
        }
    }
    fn key(source: &Source) -> SourceKey {
        SourceKey::new(source)
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
    fn native(event: &UsageEvent) -> &str {
        &event.id.native_id
    }
    fn session_of(event: &UsageEvent) -> &str {
        &event.session.native_id
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
            let path = PathBuf::from("fixture.jsonl");
            let mut parser = ParserState::new(
                AgentKind::Codex,
                "local",
                &DiscoveredFile::codex(path.clone()),
            );
            let mut cursor = JsonlCursor::default();
            let error = cursor
                .consume(
                    BufReader::new(CancellableRead::new(trigger, &stopped)),
                    &stopped,
                    |frame, line| {
                        if let Frame::Line(bytes) = frame {
                            parser.ingest(bytes, RecordLocation { path: &path, line });
                        }
                    },
                )
                .unwrap_err();
            assert!(cancel::is_cancelled(&error.into()));
            assert!(
                bytes.load(Ordering::Relaxed) <= 16 * 1024,
                "did not stop between chunks"
            );
            let ParsedFileSnapshot::Codex(snapshot) = parser.snapshot() else {
                panic!("codex parser");
            };
            assert!(snapshot.candidates.is_empty());
            assert_eq!(parser.coverage().malformed, 0);
            assert_eq!(parser.coverage().pending, 0);
        }
    }

    #[test]
    fn cancellation_is_propagated_before_discovery_and_reconciliation_and_can_retry() {
        use std::sync::atomic::Ordering;
        let stopped = Arc::new(AtomicBool::new(true));
        // Cancellation wins over a missing source: it must not become a stale-source snapshot.
        assert!(cancel::is_cancelled(
            &agents::discover(&source(Path::new("/missing-source"), "local"), &stopped)
                .unwrap_err()
        ));
        assert!(cancel::is_cancelled(
            &agents::reconcile(AgentKind::Codex, &[], &stopped).unwrap_err()
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
        assert_eq!(reader.sources[&key(&sources[0])].0.files.len(), 1);
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
        assert!(resumed
            .events
            .iter()
            .any(|event| native(event) == "response:r2"));
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
                .find(|e| native(e) == "response:r2")
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
        assert_eq!(
            scan.events[0].cache_writes,
            Some(CacheWriteBreakdown::from_ttl(43_200, 0))
        );
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
            let today = crate::cli::Cli::parse_from(["agent-usage", "models", "--today"]);
            let all = crate::cli::Cli::parse_from(["agent-usage", "models", "--all"]);
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
        let parent = scan
            .events
            .iter()
            .find(|e| session_of(e) == "parent")
            .unwrap();
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
            .find(|event| native(event) == "response:r1")
            .unwrap();
        let second = scan
            .events
            .iter()
            .find(|event| native(event) == "response:r2")
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
                    .find(|event| native(event) == format!("response:{id}"))
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
                let file = DiscoveredFile::codex(path.clone());
                if let Ok(parsed) =
                    parse_cached(&path, &metadata, None, &sources[0], &file, &serial.stopped)
                {
                    serial.files.insert((key(&sources[0]), path), parsed);
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
    fn periodic_reconciliation_preserves_claude_main_and_subagent_usage() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("projects/p");
        let subagents = project.join("session/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let record = |id: &str| {
            json!({"type":"assistant","sessionId":"session",
                "timestamp":"2026-11-01T10:00:00Z",
                "message":{"id":id,"model":"claude-sonnet-4-5",
                    "usage":{"input_tokens":10,"output_tokens":1,
                        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
        };
        let main = project.join("session.jsonl");
        let child = subagents.join("agent-child.jsonl");
        write(&main, &[record("main")]);
        write(&child, &[record("child")]);
        let mut source = source(dir.path(), "claude");
        source.agent = AgentKind::ClaudeCode;
        let sources = [source];
        let mut reader = Reader::new();
        let baseline = reader.scan(&sources).unwrap();
        assert_eq!(baseline.events.len(), 2);
        assert!(baseline.events.iter().any(|event| event.session
            == SessionKey::execution(AgentKind::ClaudeCode, "session", "child")));

        reader.reconcile_at = Some(Instant::now() - Duration::from_secs(1));
        reader.scan(&sources).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while reader.reconcile_job.is_some() || !reader.sweep.is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
            let scan = reader.scan(&sources).unwrap();
            assert_eq!(scan.events, baseline.events);
            assert_eq!(scan.coverage.uncertain, baseline.coverage.uncertain);
        }

        // Appends must continue from the correctly reconstructed parser state.
        for path in [&main, &child] {
            let mut file = File::options().append(true).open(path).unwrap();
            writeln!(file, "{}", record(&path.display().to_string())).unwrap();
        }
        let updated = reader.scan(&sources).unwrap();
        assert_eq!(updated.events.len(), 4);
        assert_eq!(
            updated
                .events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            44
        );
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
            .get_mut(&(key(&sources[0]), path.clone()))
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
    #[ignore = "read-only local schema smoke: set AGENT_USAGE_TEST_ROLLOUT"]
    fn local_rollout_read_only_smoke() {
        let path =
            PathBuf::from(std::env::var_os("AGENT_USAGE_TEST_ROLLOUT").expect("set rollout path"));
        let source = source(path.parent().unwrap_or(Path::new(".")), "local");
        let file = DiscoveredFile::codex(path.clone());
        let mut parser = ParserState::new(AgentKind::Codex, &source.name, &file);
        let mut cursor = JsonlCursor::default();
        let stopped = AtomicBool::new(false);
        cursor
            .consume(
                BufReader::new(File::open(&path).unwrap()),
                &stopped,
                |frame, line| {
                    if let Frame::Line(bytes) = frame {
                        parser.ingest(bytes, RecordLocation { path: &path, line });
                    }
                },
            )
            .unwrap();
        let ParsedFileSnapshot::Codex(snapshot) = parser.snapshot() else {
            panic!("codex parser");
        };
        assert_eq!(
            parser.coverage().malformed,
            0,
            "{:?}",
            parser.coverage().messages
        );
        let scan = crate::agents::reconcile(
            AgentKind::Codex,
            &[ParsedFileSnapshot::Codex(snapshot)],
            &stopped,
        )
        .unwrap();
        assert!(!scan.events.is_empty());
        assert_eq!(scan.coverage.conflicts, 0);
        eprintln!(
            "Read-only local smoke: {} usage increments, {} total tokens; {} uncertain",
            scan.events.len(),
            scan.events
                .iter()
                .map(|event| event.usage.total())
                .sum::<u64>(),
            scan.coverage.uncertain
        );
    }
}
