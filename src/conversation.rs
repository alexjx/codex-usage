//! Selected-session conversation loading; independent from token accounting.
use crate::{cancel, model::Source};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub session_id: String,
    pub project: Option<String>,
    pub sources: Vec<String>,
    pub files: Vec<ConversationFile>,
    pub turns: Vec<Turn>,
    pub warnings: Vec<String>,
    pub parent_session: Option<String>,
    pub pending: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct ConversationFile {
    pub source: String,
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub id: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub title: String,
    pub blocks: Vec<Block>,
    pub inherited: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct Block {
    pub id: String,
    pub kind: BlockKind,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: String,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
    pub output: Option<String>,
    pub status: Option<String>,
    pub duration_ms: Option<u64>,
    pub source: String,
    pub path: PathBuf,
    pub line: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    User,
    Assistant,
    Tool,
    Notice,
}

/// Preserve line structure but remove terminal escape/control sequences.
pub fn sanitize(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn changed(conversation: &Conversation) -> bool {
    conversation
        .files
        .iter()
        .any(|file| match fs::metadata(&file.path) {
            Ok(metadata) => {
                metadata.len() != file.len
                    || metadata.modified().ok().map(DateTime::<Utc>::from) != file.modified
            }
            Err(_) => true,
        })
}

pub fn load(sources: &[Source], session: &str, stopped: &AtomicBool) -> Result<Conversation> {
    load_conversation(sources, session, stopped)
}

const HEADER_BUDGET: usize = 1024 * 1024;
const CHUNK: usize = 32 * 1024;

struct CheckedRead<'a, R> {
    inner: R,
    stopped: &'a AtomicBool,
}
impl<R: Read> Read for CheckedRead<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(io::Error::other(cancel::Cancelled));
        }
        let count = bytes.len().min(CHUNK);
        let read = self.inner.read(&mut bytes[..count])?;
        if self.stopped.load(Ordering::Relaxed) {
            return Err(io::Error::other(cancel::Cancelled));
        }
        Ok(read)
    }
}
fn open<'a>(path: &Path, stopped: &'a AtomicBool) -> Result<Box<dyn BufRead + 'a>> {
    cancel::check(stopped)?;
    let file = CheckedRead {
        inner: File::open(path)?,
        stopped,
    };
    let input: Box<dyn Read + 'a> = if path.extension().is_some_and(|v| v == "zst") {
        Box::new(zstd::stream::read::Decoder::new(file)?)
    } else {
        Box::new(file)
    };
    Ok(Box::new(BufReader::with_capacity(
        CHUNK,
        CheckedRead {
            inner: input,
            stopped,
        },
    )))
}
#[derive(Debug, PartialEq)]
enum Line {
    Complete,
    Final,
    End,
    Limit,
}
fn read_line(
    input: &mut dyn BufRead,
    bytes: &mut Vec<u8>,
    stopped: &AtomicBool,
    limit: Option<usize>,
) -> Result<Line> {
    bytes.clear();
    loop {
        cancel::check(stopped)?;
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok(if bytes.is_empty() {
                Line::End
            } else {
                Line::Final
            });
        }
        let end = available.iter().position(|b| *b == b'\n').map(|n| n + 1);
        let count = end
            .unwrap_or(available.len())
            .min(limit.unwrap_or(usize::MAX).saturating_sub(bytes.len()));
        if count == 0 {
            return Ok(Line::Limit);
        }
        bytes.extend_from_slice(&available[..count]);
        let complete = end.is_some_and(|end| count == end);
        input.consume(count);
        if complete {
            return Ok(Line::Complete);
        }
        if limit.is_some_and(|limit| bytes.len() >= limit) {
            return Ok(Line::Limit);
        }
    }
}
fn string(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).map(str::to_owned)
}
fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}
#[derive(Default)]
struct Header {
    owner: String,
    parent: Option<String>,
    project: Option<String>,
    created: Option<DateTime<Utc>>,
}
fn session_header(path: &Path, stopped: &AtomicBool) -> Result<Option<Header>> {
    let mut input = open(path, stopped)?;
    let mut bytes = Vec::new();
    let mut remaining = HEADER_BUDGET;
    for _ in 0..64 {
        let line = read_line(input.as_mut(), &mut bytes, stopped, Some(remaining))?;
        if line == Line::End || line == Line::Limit {
            break;
        }
        remaining = remaining.saturating_sub(bytes.len());
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if value["type"] != "session_meta" {
            continue;
        }
        let payload = &value["payload"];
        let Some(owner) = string(payload, "id")
            .or_else(|| string(payload, "session_id"))
            .or_else(|| string(payload, "thread_id"))
        else {
            continue;
        };
        let parent = string(payload, "forked_from_id")
            .or_else(|| string(payload, "parent_thread_id"))
            .or_else(|| string(&payload["forked_from_id"], "session_id"));
        return Ok(Some(Header {
            owner,
            parent,
            project: string(payload, "cwd"),
            created: timestamp(payload).or_else(|| timestamp(&value)),
        }));
    }
    Ok(None)
}
fn warning(warnings: &mut Vec<String>, text: impl Into<String>) {
    let text = sanitize(&text.into());
    if warnings.len() < 64 && !warnings.contains(&text) {
        warnings.push(text);
    }
}

#[derive(Clone)]
struct Entry {
    turn: String,
    inherited: bool,
    block: Block,
    canonical: bool,
    stable_id: bool,
    order: Option<DateTime<Utc>>,
}
struct Output {
    call: String,
    text: String,
    status: Option<String>,
    turn: String,
    timestamp: Option<DateTime<Utc>>,
    source: String,
    path: PathBuf,
    line: u64,
}
struct Parsed {
    hash: blake3::Hasher,
    entries: VecDeque<Entry>,
    outputs: Vec<Output>,
    lifecycle: BTreeMap<String, (Option<u64>, Option<String>)>,
    aborted: BTreeSet<String>,
    pending: usize,
    warnings: Vec<String>,
}
struct Parser<'a> {
    session: &'a str,
    source: &'a str,
    path: &'a Path,
    header: &'a Header,
    turn: String,
    explicit_turn: bool,
    inherited: bool,
    model: Option<String>,
    mode: Option<String>,
    implicit: usize,
    last_order: Option<DateTime<Utc>>,
    parsed: Parsed,
}
fn mode(value: Option<String>) -> Option<String> {
    value.map(|v| match v.as_str() {
        "priority" | "fast" => "fast".into(),
        "default" | "standard" => "regular".into(),
        _ => v,
    })
}
fn uuid_millis(id: &str) -> Option<i64> {
    let compact: String = id.chars().filter(|c| *c != '-').collect();
    if compact.len() != 32 || compact.as_bytes().get(12) != Some(&b'7') {
        return None;
    }
    i64::from_str_radix(&compact[..12], 16).ok()
}
fn content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return sanitize(text);
    }
    let Some(parts) = value.as_array() else {
        return "[Message text unavailable in this log]".into();
    };
    let mut result = Vec::new();
    for part in parts {
        let kind = part["type"].as_str().unwrap_or("");
        if let Some(text) = part["text"].as_str() {
            result.push(sanitize(text));
        } else if matches!(kind, "input_image" | "image" | "image_url" | "output_image") {
            result.push("[Image attachment]".into());
        } else if matches!(kind, "input_file" | "file" | "file_attachment") {
            let name = part["filename"]
                .as_str()
                .or_else(|| part["name"].as_str())
                .unwrap_or("file");
            result.push(format!("[File attachment: {}]", sanitize(name)));
        } else if kind.contains("audio") {
            result.push("[Audio attachment]".into());
        } else if kind == "encrypted_content" {
            result.push("[Message text unavailable in this log]".into());
        }
    }
    if result.is_empty() {
        "[Message text unavailable in this log]".into()
    } else {
        result.join("\n")
    }
}
fn text_value(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        sanitize(text)
    } else if value
        .as_array()
        .is_some_and(|parts| parts.iter().any(|part| part.get("type").is_some()))
    {
        content(value)
    } else if value.get("content").is_some_and(Value::is_array) {
        content(&value["content"])
    } else {
        sanitize(&serde_json::to_string_pretty(value).unwrap_or_default())
    }
}
impl<'a> Parser<'a> {
    fn new(session: &'a str, source: &'a str, path: &'a Path, header: &'a Header) -> Self {
        Self {
            session,
            source,
            path,
            header,
            turn: "unattributed".into(),
            explicit_turn: false,
            inherited: false,
            model: None,
            mode: None,
            implicit: 0,
            last_order: None,
            parsed: Parsed {
                hash: blake3::Hasher::new(),
                entries: VecDeque::new(),
                outputs: Vec::new(),
                lifecycle: BTreeMap::new(),
                aborted: BTreeSet::new(),
                pending: 0,
                warnings: Vec::new(),
            },
        }
    }
    fn own_record(&self, payload: &Value) -> bool {
        let owner = payload["thread_id"]
            .as_str()
            .or_else(|| payload["session_id"].as_str());
        owner.is_none_or(|id| id == self.session || self.header.parent.as_deref() == Some(id))
    }
    fn set_turn(&mut self, payload: &Value) {
        if let Some(turn) = string(payload, "turn_id").or_else(|| string(payload, "root_turn_id")) {
            self.inherited = payload["thread_id"]
                .as_str()
                .is_some_and(|id| self.header.parent.as_deref() == Some(id))
                || self.header.parent.is_some()
                    && uuid_millis(&turn)
                        .zip(self.header.created)
                        .is_some_and(|(millis, created)| millis < created.timestamp_millis());
            self.turn = turn;
            self.explicit_turn = true;
        }
    }
    fn block(
        &mut self,
        payload: &Value,
        time: Option<DateTime<Utc>>,
        line: u64,
        canonical: bool,
        kind: BlockKind,
        text: String,
    ) {
        if kind == BlockKind::User && !self.explicit_turn {
            let mirrored = self.parsed.entries.back().is_some_and(|last| {
                last.block.kind == BlockKind::User
                    && last.block.text == text
                    && last.canonical != canonical
            });
            if !mirrored {
                self.implicit += 1;
                self.turn = format!("implicit:{}:{}", self.path.display(), self.implicit);
            }
        }
        let call_id = string(payload, "call_id");
        let stable = string(payload, "id").or_else(|| string(payload, "message_id"));
        let id = stable
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.path.display(), line));
        self.parsed.entries.push_back(Entry {
            turn: self.turn.clone(),
            inherited: self.inherited,
            canonical,
            stable_id: stable.is_some(),
            order: time.or(self.last_order),
            block: Block {
                id,
                kind,
                timestamp: time,
                text,
                model: self.model.clone(),
                mode: self.mode.clone(),
                call_id,
                tool_name: string(payload, "name"),
                output: None,
                status: string(payload, "status"),
                duration_ms: payload["duration_ms"].as_u64(),
                source: self.source.into(),
                path: self.path.into(),
                line,
            },
        });
    }
    fn item(&mut self, payload: &Value, time: Option<DateTime<Utc>>, line: u64, canonical: bool) {
        match payload["type"].as_str().unwrap_or("") {
            "message" => {
                let kind = match payload["role"].as_str() {
                    Some("user") => BlockKind::User,
                    Some("assistant") => BlockKind::Assistant,
                    _ => return,
                };
                self.block(
                    payload,
                    time,
                    line,
                    canonical,
                    kind,
                    content(&payload["content"]),
                );
            }
            "function_call" | "custom_tool_call" => {
                let text = text_value(
                    payload
                        .get("arguments")
                        .or_else(|| payload.get("input"))
                        .unwrap_or(&Value::Null),
                );
                self.block(payload, time, line, canonical, BlockKind::Tool, text);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let Some(call) = string(payload, "call_id") else {
                    warning(&mut self.parsed.warnings, "Tool result lacks a call ID");
                    return;
                };
                self.parsed.outputs.push(Output {
                    call,
                    text: text_value(&payload["output"]),
                    status: string(payload, "status"),
                    turn: self.turn.clone(),
                    timestamp: time,
                    source: self.source.into(),
                    path: self.path.into(),
                    line,
                });
            }
            _ => {}
        }
    }
    fn record(&mut self, value: &Value, line: u64) {
        let payload = &value["payload"];
        let time = timestamp(value).or_else(|| timestamp(payload));
        if time.is_some() {
            self.last_order = time;
        }
        if !self.own_record(payload) {
            return;
        }
        match value["type"].as_str().unwrap_or("") {
            "turn_context" => {
                self.set_turn(payload);
                if let Some(model) = string(payload, "model") {
                    self.model = Some(model);
                }
                if payload.get("service_tier").is_some() {
                    self.mode = mode(string(payload, "service_tier"));
                }
            }
            "thread_settings_applied" => {
                self.mode = mode(string(&payload["thread_settings"], "service_tier"))
            }
            "response_item" => self.item(payload, time, line, true),
            "event_msg" => match payload["type"].as_str().unwrap_or("") {
                "task_started" => self.set_turn(payload),
                "thread_settings_applied" => {
                    self.mode = mode(string(&payload["thread_settings"], "service_tier"))
                }
                "user_message" | "agent_message" => {
                    if payload.get("turn_id").is_some() {
                        self.set_turn(payload);
                    }
                    let kind = if payload["type"] == "user_message" {
                        BlockKind::User
                    } else {
                        BlockKind::Assistant
                    };
                    let text = payload
                        .get("message")
                        .or_else(|| payload.get("text"))
                        .map(text_value)
                        .unwrap_or_else(|| "[Message text unavailable in this log]".into());
                    self.block(payload, time, line, false, kind, text);
                }
                "item_completed" => {
                    let item = &payload["item"];
                    let duration = payload["completed_at_ms"]
                        .as_u64()
                        .zip(payload["started_at_ms"].as_u64())
                        .and_then(|(end, start)| end.checked_sub(start));
                    if let Some(id) = string(item, "call_id").or_else(|| string(item, "id")) {
                        self.parsed
                            .lifecycle
                            .insert(id, (duration, string(item, "status")));
                    }
                    // Item lifecycle wrappers mirror response_item, where available.
                    let previous = self.turn.clone();
                    let explicit = self.explicit_turn;
                    let inherited = self.inherited;
                    self.set_turn(payload);
                    self.item(item, time, line, false);
                    self.turn = previous;
                    self.explicit_turn = explicit;
                    self.inherited = inherited;
                }
                "turn_aborted" => {
                    let turn = string(payload, "turn_id").unwrap_or_else(|| self.turn.clone());
                    self.parsed.aborted.insert(turn);
                }
                _ => {}
            },
            _ => {}
        }
    }
    fn read(mut self, stopped: &AtomicBool) -> Result<Parsed> {
        let mut input = open(self.path, stopped)?;
        let mut bytes = Vec::new();
        let mut line_number = 0;
        loop {
            let line = read_line(input.as_mut(), &mut bytes, stopped, None)?;
            if line == Line::End {
                break;
            }
            line_number += 1;
            self.parsed.hash.update(&bytes);
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => self.record(&value, line_number),
                Err(error) if line == Line::Final && error.is_eof() => self.parsed.pending += 1,
                Err(_) => warning(
                    &mut self.parsed.warnings,
                    format!(
                        "{}:{line_number}: malformed conversation record",
                        self.path.display()
                    ),
                ),
            }
            if line == Line::Final {
                break;
            }
        }
        Ok(self.parsed)
    }
}

fn merge_block(target: &mut Block, other: &Block) {
    if target.model.is_none() {
        target.model.clone_from(&other.model);
    }
    if target.mode.is_none() {
        target.mode.clone_from(&other.mode);
    }
    if target.status.is_none() {
        target.status.clone_from(&other.status);
    }
    if target.duration_ms.is_none() {
        target.duration_ms = other.duration_ms;
    }
    if target.timestamp.is_none() {
        target.timestamp = other.timestamp;
    }
}
fn load_conversation(
    sources: &[Source],
    session: &str,
    stopped: &AtomicBool,
) -> Result<Conversation> {
    cancel::check(stopped)?;
    if session.is_empty() {
        bail!("A session ID is required to read its conversation");
    }
    let mut conversation = Conversation {
        session_id: session.into(),
        project: None,
        sources: Vec::new(),
        files: Vec::new(),
        turns: Vec::new(),
        warnings: Vec::new(),
        parent_session: None,
        pending: 0,
    };
    let mut files = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for source in sources {
        cancel::check(stopped)?;
        if !source.root.is_dir() {
            warning(
                &mut conversation.warnings,
                format!("Source {} is unavailable", source.name),
            );
            continue;
        }
        let mut directories: Vec<_> = [
            source.root.join("sessions"),
            source.root.join("archived_sessions"),
        ]
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
        if directories.is_empty() {
            directories.push(source.root.clone());
        }
        for directory in directories {
            for entry in WalkDir::new(&directory)
                .follow_links(false)
                .sort_by_file_name()
            {
                cancel::check(stopped)?;
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        warning(
                            &mut conversation.warnings,
                            format!("Cannot inspect {}: {e}", directory.display()),
                        );
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if !name.ends_with(".jsonl") && !name.ends_with(".jsonl.zst") {
                    continue;
                }
                if !seen_paths.insert((source.name.clone(), path.to_owned())) {
                    continue;
                }
                let hint = name.contains(session);
                let header = match session_header(path, stopped) {
                    Ok(Some(header)) if header.owner == session => header,
                    Ok(Some(_)) => continue,
                    Ok(None) if hint => {
                        warning(
                            &mut conversation.warnings,
                            format!(
                                "{}: session metadata unavailable; selected by filename",
                                path.display()
                            ),
                        );
                        Header {
                            owner: session.into(),
                            ..Default::default()
                        }
                    }
                    Ok(None) => continue,
                    Err(error) if cancel::is_cancelled(&error) => return Err(error),
                    Err(error) => {
                        if hint {
                            warning(
                                &mut conversation.warnings,
                                format!("Cannot read {}: {error}", path.display()),
                            );
                        }
                        continue;
                    }
                };
                files.push((source.name.clone(), path.to_owned(), header));
            }
        }
    }
    if files.is_empty() {
        bail!(
            "No retained conversation files found for session {} in the selected sources",
            sanitize(session)
        );
    }
    let mut streams = Vec::new();
    let mut outputs = Vec::new();
    let mut lifecycle = BTreeMap::new();
    let mut aborted = BTreeSet::new();
    let mut hashes = BTreeSet::new();
    for (source, path, header) in files {
        cancel::check(stopped)?;
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(error) => {
                warning(
                    &mut conversation.warnings,
                    format!("Cannot inspect {}: {error}", path.display()),
                );
                continue;
            }
        };
        match Parser::new(session, &source, &path, &header).read(stopped) {
            Ok(parsed) => {
                if conversation.project.is_none() {
                    conversation.project = header.project.map(|p| sanitize(&p));
                }
                if conversation.parent_session.is_none() {
                    conversation.parent_session = header.parent;
                }
                if !conversation.sources.contains(&source) {
                    conversation.sources.push(source.clone());
                }
                conversation.files.push(ConversationFile {
                    source,
                    path,
                    len: metadata.len(),
                    modified: metadata.modified().ok().map(DateTime::<Utc>::from),
                });
                for message in parsed.warnings {
                    warning(&mut conversation.warnings, message);
                }
                // Identical file copies are demonstrably duplicates, including old records without IDs.
                if !hashes.insert(*parsed.hash.finalize().as_bytes()) {
                    continue;
                }
                conversation.pending += parsed.pending;
                streams.push(parsed.entries);
                outputs.extend(parsed.outputs);
                lifecycle.extend(parsed.lifecycle);
                aborted.extend(parsed.aborted);
            }
            Err(error) if cancel::is_cancelled(&error) => return Err(error),
            Err(error) => warning(
                &mut conversation.warnings,
                format!("Cannot load {}: {error}", path.display()),
            ),
        }
    }
    if conversation.files.is_empty() {
        bail!(
            "Cannot read conversation for session {}: {}",
            sanitize(session),
            conversation.warnings.join("; ")
        );
    }
    let mut canonical: BTreeMap<_, Vec<u64>> = BTreeMap::new();
    for entry in streams
        .iter()
        .flat_map(|stream| stream.iter())
        .filter(|e| e.canonical)
    {
        canonical
            .entry((
                entry.block.path.clone(),
                entry.turn.clone(),
                kind_key(entry.block.kind),
                *blake3::hash(entry.block.text.as_bytes()).as_bytes(),
            ))
            .or_default()
            .push(entry.block.line);
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut identities = BTreeMap::new();
    while streams.iter().any(|s| !s.is_empty()) {
        cancel::check(stopped)?;
        // Merge source heads without rearranging the original order within any file.
        let index = streams
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.front().map(|e| (i, e.order)))
            .min_by_key(|(i, time)| (*time, *i))
            .unwrap()
            .0;
        let mut entry = streams[index].pop_front().unwrap();
        if !entry.canonical {
            let key = (
                entry.block.path.clone(),
                entry.turn.clone(),
                kind_key(entry.block.kind),
                *blake3::hash(entry.block.text.as_bytes()).as_bytes(),
            );
            if let Some(candidates) = canonical.get_mut(&key) {
                // Match each canonical occurrence to at most one nearby mirror.
                // Text equality across a turn alone must not erase repeated prompts.
                if let Some((index, _)) = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.abs_diff(entry.block.line) <= 8)
                    .min_by_key(|(_, line)| line.abs_diff(entry.block.line))
                {
                    candidates.remove(index);
                    continue;
                }
            }
        }
        let identity = if entry.block.kind == BlockKind::Tool {
            entry.block.call_id.as_ref().map(|id| format!("tool:{id}"))
        } else if entry.stable_id {
            Some(format!("{}:{}", kind_key(entry.block.kind), entry.block.id))
        } else {
            None
        };
        if let Some(ref identity) = identity {
            if let Some(&previous) = identities.get(identity) {
                let previous: &mut Entry = &mut entries[previous];
                if previous.block.text == entry.block.text
                    && previous.block.kind == entry.block.kind
                {
                    if previous
                        .block
                        .model
                        .as_ref()
                        .zip(entry.block.model.as_ref())
                        .is_some_and(|(a, b)| a != b)
                        || previous
                            .block
                            .mode
                            .as_ref()
                            .zip(entry.block.mode.as_ref())
                            .is_some_and(|(a, b)| a != b)
                    {
                        warning(
                            &mut conversation.warnings,
                            format!(
                                "Conflicting model/mode metadata for conversation item {}",
                                entry.block.id
                            ),
                        );
                    }
                    merge_block(&mut previous.block, &entry.block);
                    continue;
                }
                warning(
                    &mut conversation.warnings,
                    format!(
                        "Conflicting copies of conversation item {}; both retained",
                        entry.block.id
                    ),
                );
                entry.block.id = format!(
                    "{}:conflict:{}:{}",
                    entry.block.id,
                    entry.block.path.display(),
                    entry.block.line
                );
            } else {
                identities.insert(identity.clone(), entries.len());
            }
        }
        entries.push(entry);
    }
    let mut call_indices = BTreeMap::new();
    for (index, entry) in entries.iter_mut().enumerate() {
        if let Some(call) = &entry.block.call_id {
            call_indices
                .entry(call.clone())
                .and_modify(|value| *value = None)
                .or_insert(Some(index));
        }
        let timing = entry
            .block
            .call_id
            .as_ref()
            .and_then(|call| lifecycle.get(call))
            .or_else(|| lifecycle.get(&entry.block.id));
        if let Some((duration, status)) = timing {
            if duration.is_some() {
                entry.block.duration_ms = *duration;
            }
            if status.is_some() {
                entry.block.status.clone_from(status);
            }
        }
    }
    let mut output_seen = BTreeSet::new();
    for output in outputs {
        cancel::check(stopped)?;
        if !output_seen.insert((
            output.call.clone(),
            *blake3::hash(output.text.as_bytes()).as_bytes(),
        )) {
            continue;
        }
        if let Some(&Some(index)) = call_indices.get(&output.call) {
            let block = &mut entries[index].block;
            if let Some(existing) = &block.output {
                if existing != &output.text {
                    warning(
                        &mut conversation.warnings,
                        format!(
                            "Multiple distinct results for tool {}; retained together",
                            output.call
                        ),
                    );
                    block.output = Some(format!(
                        "{existing}\n\n[Additional retained result]\n{}",
                        output.text
                    ));
                }
            } else {
                block.output = Some(output.text);
            }
            if output.status.is_some() {
                block.status = output.status;
            }
        } else {
            warning(
                &mut conversation.warnings,
                "A tool result has no retained matching call",
            );
            entries.push(Entry {
                turn: output.turn,
                inherited: false,
                canonical: true,
                stable_id: false,
                order: output.timestamp,
                block: Block {
                    id: format!("orphan-output:{}:{}", output.path.display(), output.line),
                    kind: BlockKind::Notice,
                    timestamp: output.timestamp,
                    text: format!(
                        "[Tool result without matching call: {}]\n{}",
                        sanitize(&output.call),
                        output.text
                    ),
                    model: None,
                    mode: None,
                    call_id: Some(output.call),
                    tool_name: None,
                    output: None,
                    status: output.status,
                    duration_ms: None,
                    source: output.source,
                    path: output.path,
                    line: output.line,
                },
            });
        }
    }
    let mut turns = BTreeMap::new();
    for entry in entries {
        cancel::check(stopped)?;
        let key = (entry.inherited, entry.turn.clone());
        let index = *turns.entry(key).or_insert_with(|| {
            let index = conversation.turns.len();
            conversation.turns.push(Turn {
                id: entry.turn.clone(),
                timestamp: entry.block.timestamp,
                title: if entry.inherited {
                    "Earlier context".into()
                } else {
                    "Conversation".into()
                },
                blocks: Vec::new(),
                inherited: entry.inherited,
            });
            index
        });
        let turn = &mut conversation.turns[index];
        let mut block = entry.block;
        if block.kind == BlockKind::User && turn.title == "Conversation" {
            turn.title = block
                .text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("User message")
                .chars()
                .take(160)
                .collect();
        }
        if turn.timestamp.is_none() {
            turn.timestamp = block.timestamp;
        }
        if block.kind == BlockKind::Tool && block.output.is_none() && aborted.contains(&entry.turn)
        {
            block.status = Some("interrupted".into());
        }
        turn.blocks.push(block);
    }
    if conversation.turns.is_empty() {
        warning(
            &mut conversation.warnings,
            "No readable user or assistant messages are retained in these logs",
        );
    }
    if conversation.parent_session.is_some() && !conversation.turns.iter().any(|t| t.inherited) {
        warning(
            &mut conversation.warnings,
            "Parent conversation context is not retained or cannot be identified",
        );
    }
    conversation.sources.sort();
    Ok(conversation)
}
fn kind_key(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::User => "user",
        BlockKind::Assistant => "assistant",
        BlockKind::Tool => "tool",
        BlockKind::Notice => "notice",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::{tempdir, TempDir};

    struct Fixture {
        dir: TempDir,
        sources: Vec<Source>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let root = dir.path().join("logs");
            fs::create_dir_all(root.join("sessions")).unwrap();
            Self {
                dir,
                sources: vec![Source {
                    name: "local".into(),
                    kind: "linux".into(),
                    root,
                    enabled: true,
                }],
            }
        }
        fn write(&self, name: &str, records: &[Value]) -> PathBuf {
            let path = self.sources[0].root.join("sessions").join(name);
            let mut file = File::create(&path).unwrap();
            for value in records {
                writeln!(file, "{value}").unwrap();
            }
            path
        }
        fn load(&self) -> Conversation {
            load(&self.sources, "selected", &AtomicBool::new(false)).unwrap()
        }
    }
    fn meta(id: &str) -> Value {
        json!({"type":"session_meta","timestamp":"2026-11-04T12:00:00Z","payload":{"id":id,"cwd":"/fixture"}})
    }
    fn turn(id: &str) -> Value {
        json!({"type":"turn_context","payload":{"turn_id":id,"model":"gpt-test","service_tier":"priority"}})
    }
    fn message(id: &str, role: &str, text: &str) -> Value {
        json!({"type":"response_item","timestamp":"2026-11-04T12:00:01Z","payload":{"type":"message","id":id,"role":role,"content":[{"type":"input_text","text":text}]}})
    }
    fn all(c: &Conversation) -> Vec<&Block> {
        c.turns.iter().flat_map(|t| &t.blocks).collect()
    }

    #[test]
    fn canonical_mirrors_and_copies_keep_legitimate_repeated_prompts() {
        let f = Fixture::new();
        let records = vec![
            meta("selected"),
            turn("one"),
            message("u1", "user", "Again"),
            json!({"type":"event_msg","payload":{"type":"user_message","turn_id":"one","message":"Again"}}),
            message("a1", "assistant", "First answer"),
            turn("two"),
            message("u2", "user", "Again"),
            message("a2", "assistant", "Second answer"),
        ];
        f.write("first.jsonl", &records);
        let mut copy = records.clone();
        copy.push(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"two"}}));
        f.write("copy.jsonl", &copy);
        let c = f.load();
        assert_eq!(c.turns.len(), 2);
        assert_eq!(all(&c).len(), 4);
        assert_eq!(
            all(&c)
                .iter()
                .filter(|b| b.kind == BlockKind::User && b.text == "Again")
                .count(),
            2
        );
        assert_eq!(c.turns[0].blocks[0].mode.as_deref(), Some("fast"));
        assert_eq!(c.files.len(), 2);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    }

    #[test]
    fn mirrors_consume_only_one_local_occurrence_and_keep_repeated_fallback_messages() {
        let f = Fixture::new();
        let fallback = || json!({"type":"event_msg","payload":{"type":"user_message","turn_id":"one","message":"Again"}});
        f.write(
            "repeat.jsonl",
            &[
                meta("selected"),
                turn("one"),
                message("u1", "user", "Again"),
                fallback(),
                fallback(),
            ],
        );
        let c = f.load();
        assert_eq!(
            all(&c).iter().filter(|b| b.kind == BlockKind::User).count(),
            2
        );
        f.write(
            "repeat.jsonl",
            &[
                meta("selected"),
                turn("one"),
                message("u1", "user", "Again"),
                fallback(),
                message("u2", "user", "Again"),
                fallback(),
            ],
        );
        let c = f.load();
        assert_eq!(
            all(&c).iter().filter(|b| b.kind == BlockKind::User).count(),
            2
        );
    }

    #[test]
    fn legacy_events_remain_visible_and_unrelated_thread_messages_are_excluded() {
        let f = Fixture::new();
        f.write("generic.jsonl",&[meta("selected"),turn("one"),json!({"type":"event_msg","payload":{"type":"user_message","message":"Legacy prompt"}}),json!({"type":"event_msg","payload":{"type":"agent_message","message":"Legacy answer"}}),json!({"type":"event_msg","payload":{"type":"agent_message","thread_id":"different-agent","message":"Do not show"}}),turn("two"),json!({"type":"event_msg","payload":{"type":"user_message","message":"Legacy prompt"}})]);
        let c = f.load();
        assert_eq!(c.turns.len(), 2);
        assert_eq!(all(&c).len(), 3);
        assert!(!all(&c).iter().any(|b| b.text.contains("Do not show")));
    }

    #[test]
    fn tool_outputs_attach_by_id_even_when_result_precedes_call_and_use_recorded_timing() {
        let f = Fixture::new();
        f.write("tools.jsonl",&[meta("selected"),turn("one"),message("u","user","Use tools"),json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c","output":"hello"}}),json!({"type":"response_item","payload":{"type":"custom_tool_call","id":"item","call_id":"c","name":"apply_patch","input":"patch"}}),json!({"type":"event_msg","payload":{"type":"item_completed","started_at_ms":100,"completed_at_ms":275,"turn_id":"one","item":{"type":"custom_tool_call","id":"item","call_id":"c","name":"apply_patch","input":"patch","status":"completed"}}})]);
        let c = f.load();
        let b = all(&c)
            .into_iter()
            .find(|b| b.kind == BlockKind::Tool)
            .unwrap();
        assert_eq!(b.output.as_deref(), Some("hello"));
        assert_eq!(b.duration_ms, Some(175));
        assert_eq!(b.status.as_deref(), Some("completed"));
        assert_eq!(all(&c).len(), 2);
    }

    #[test]
    fn only_lifecycle_evidence_marks_missing_tool_output_interrupted() {
        let f = Fixture::new();
        f.write("tools.jsonl",&[meta("selected"),turn("one"),json!({"type":"response_item","payload":{"type":"function_call","call_id":"c","name":"exec_command","arguments":"{}"}}),json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"one"}}),turn("two"),json!({"type":"response_item","payload":{"type":"function_call","call_id":"d","name":"exec_command","arguments":"{}"}})]);
        let c = f.load();
        assert_eq!(all(&c)[0].status.as_deref(), Some("interrupted"));
        assert_eq!(all(&c)[1].status, None);
    }

    #[test]
    fn fork_turn_identity_distinguishes_parent_context_despite_rewritten_timestamps() {
        let f = Fixture::new();
        let created: DateTime<Utc> = "2026-11-04T12:00:00Z".parse().unwrap();
        let id = |millis: i64| {
            let s = format!("{millis:012x}");
            format!("{}-{}-7000-8000-000000000001", &s[..8], &s[8..])
        };
        let mut header = meta("selected");
        header["payload"]["parent_thread_id"] = json!("parent");
        f.write(
            "fork.jsonl",
            &[
                header,
                turn(&id(created.timestamp_millis() - 1000)),
                message("old", "user", "Inherited question"),
                turn(&id(created.timestamp_millis() + 1000)),
                message("new", "user", "Current question"),
            ],
        );
        let c = f.load();
        assert_eq!(c.parent_session.as_deref(), Some("parent"));
        assert!(c.turns[0].inherited);
        assert!(!c.turns[1].inherited);
        assert_eq!(c.turns[0].blocks[0].text, "Inherited question");
    }

    #[test]
    fn attachments_and_missing_content_are_labels_and_escape_sequences_are_removed() {
        let f = Fixture::new();
        f.write("attachments.jsonl",&[meta("selected"),turn("one"),json!({"type":"response_item","payload":{"type":"message","id":"u","role":"user","content":[{"type":"input_text","text":"你好\n\u{1b}[31mred\u{1b}[0m\u{1b}]52;c;secret\u{7}"},{"type":"input_image","image_url":"data:image/png;base64,VERY_PRIVATE"},{"type":"input_file","filename":"report.txt"}]}}),json!({"type":"response_item","payload":{"type":"message","id":"a","role":"assistant","content":[{"type":"encrypted_content","data":"DO_NOT_PRINT"}]}}),message("s","system","hidden instruction"),json!({"type":"response_item","payload":{"type":"reasoning","summary":"hidden reasoning"}})]);
        let c = f.load();
        let blocks = all(&c);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].text.contains("你好\nred"));
        assert!(blocks[0].text.contains("[Image attachment]"));
        assert!(blocks[0].text.contains("report.txt"));
        assert!(
            !blocks[0].text.contains("VERY_PRIVATE")
                && !blocks[0].text.contains("secret")
                && !blocks[0].text.contains('\x1b')
        );
        assert!(blocks[1].text.contains("unavailable"));
        let tool_result = text_value(
            &json!({"content":[{"type":"text","text":"Tool text"},{"type":"image","data":"PRIVATE_IMAGE_BYTES"}]}),
        );
        assert_eq!(tool_result, "Tool text\n[Image attachment]");
    }

    #[test]
    fn compressed_archives_pending_records_and_change_detection_are_independent_of_mtime_filter() {
        let f = Fixture::new();
        let archive = f.sources[0].root.join("archived_sessions");
        fs::create_dir(&archive).unwrap();
        let bytes = format!(
            "{}\n{}\n{}\n",
            meta("selected"),
            turn("one"),
            message("u", "user", "Archived")
        );
        let path = archive.join("old.jsonl.zst");
        fs::write(
            &path,
            zstd::stream::encode_all(bytes.as_bytes(), 1).unwrap(),
        )
        .unwrap();
        let pending = f.write("current.jsonl", &[meta("selected")]);
        let mut file = fs::OpenOptions::new().append(true).open(&pending).unwrap();
        write!(file, "{{\"type\":\"response_item\"").unwrap();
        drop(file);
        let c = f.load();
        assert_eq!(c.pending, 1);
        assert_eq!(all(&c).len(), 1);
        assert!(!changed(&c));
        fs::OpenOptions::new()
            .append(true)
            .open(pending)
            .unwrap()
            .write_all(b" ")
            .unwrap();
        assert!(changed(&c));
    }

    #[test]
    fn cancellation_checks_discovery_and_every_chunk_of_an_unterminated_record() {
        let f = Fixture::new();
        let stopped = AtomicBool::new(true);
        let error = load(&f.sources, "selected", &stopped).unwrap_err();
        assert!(cancel::is_cancelled(&error));
        struct StopAfterRead<'a> {
            stopped: &'a AtomicBool,
        }
        impl Read for StopAfterRead<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'x');
                self.stopped.store(true, Ordering::Relaxed);
                Ok(buf.len())
            }
        }
        let stopped = AtomicBool::new(false);
        let mut input = BufReader::new(CheckedRead {
            inner: StopAfterRead { stopped: &stopped },
            stopped: &stopped,
        });
        let error = read_line(&mut input, &mut Vec::new(), &stopped, None).unwrap_err();
        assert!(cancel::is_cancelled(&error));
    }

    #[test]
    fn source_selection_and_metadata_prevent_filename_substring_false_matches() {
        let f = Fixture::new();
        f.write(
            "selected-fake.jsonl",
            &[meta("other"), message("u", "user", "Wrong session")],
        );
        assert!(load(&f.sources, "selected", &AtomicBool::new(false)).is_err());
        f.write(
            "selected-real.jsonl",
            &[meta("selected"), message("u", "user", "Right session")],
        );
        let c = f.load();
        assert_eq!(c.files.len(), 1);
        assert_eq!(all(&c)[0].text, "Right session");
        assert!(f.dir.path().is_dir());
    }
}
