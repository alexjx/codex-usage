//! Claude Code conversation loading.
//!
//! Main transcripts live at `projects/<project>/<session>.jsonl`; subagent
//! transcripts at `projects/<project>/<session>/subagents/agent-<execution>.jsonl`.
//! Copied records merge by UUID and execution; usage parsing is a separate path.
use crate::{
    cancel,
    conversation::{sanitize, Block, BlockKind, Conversation, ConversationFile, Turn},
    model::{AgentKind, SessionKey, Source},
};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use walkdir::WalkDir;

struct CheckedRead<'a, R> {
    inner: R,
    stopped: &'a AtomicBool,
}
impl<R: Read> Read for CheckedRead<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(io::Error::other(cancel::Cancelled));
        }
        let read = self.inner.read(bytes)?;
        if self.stopped.load(Ordering::Relaxed) {
            return Err(io::Error::other(cancel::Cancelled));
        }
        Ok(read)
    }
}

fn warning(warnings: &mut Vec<String>, text: impl Into<String>) {
    let text = sanitize(&text.into());
    if warnings.len() < 64 && !warnings.contains(&text) {
        warnings.push(text);
    }
}
fn string(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).map(str::to_owned)
}
fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok())
}
fn text_of(value: &Value) -> String {
    value.as_str().map(sanitize).unwrap_or_default()
}

#[derive(Clone)]
struct Entry {
    key: String,
    parent: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    order: usize,
    blocks: Vec<Block>,
    user_prompt: bool,
}
struct Output {
    call: String,
    text: String,
    status: Option<String>,
    path: PathBuf,
    line: u64,
}
#[derive(Default)]
struct FileParse {
    hash: [u8; 32],
    entries: Vec<Entry>,
    outputs: Vec<Output>,
    project: Option<String>,
    warnings: Vec<String>,
    pending: usize,
    records: usize,
    ownership_ok: bool,
}

fn describe_attachments(part: &Value) -> Option<String> {
    match part["type"].as_str().unwrap_or("") {
        "image" | "input_image" | "image_url" | "output_image" => Some("[Image attachment]".into()),
        "document" | "file" | "input_file" | "file_attachment" => {
            let name = part["filename"]
                .as_str()
                .or_else(|| part["name"].as_str())
                .unwrap_or("file");
            Some(format!("[File attachment: {}]", sanitize(name)))
        }
        "audio" | "input_audio" => Some("[Audio attachment]".into()),
        _ => None,
    }
}

/// Parse one assistant/user message into blocks and tool results.
#[allow(clippy::too_many_arguments)]
fn message_blocks(
    message: &Value,
    id: &str,
    timestamp: Option<DateTime<Utc>>,
    source: &str,
    path: &Path,
    line: u64,
    model: Option<String>,
    mode: Option<String>,
    outputs: &mut Vec<Output>,
    warnings: &mut Vec<String>,
    user_prompt: &mut bool,
) -> Vec<Block> {
    let role = message["role"].as_str().unwrap_or("");
    let content = &message["content"];
    let mut texts = Vec::new();
    let mut blocks = Vec::new();
    let mut parts: Vec<&Value> = match content.as_array() {
        Some(parts) => parts.iter().collect(),
        None if content.is_string() => Vec::new(),
        None => Vec::new(),
    };
    let mut index = 0usize;
    let mut next_block = |kind: BlockKind,
                          text: String,
                          call_id: Option<String>,
                          tool_name: Option<String>|
     -> Block {
        let block = Block {
            id: format!("{id}:{index}"),
            kind,
            timestamp,
            text,
            model: model.clone(),
            mode: mode.clone(),
            call_id,
            tool_name,
            output: None,
            status: None,
            duration_ms: None,
            source: source.into(),
            path: path.into(),
            line,
        };
        index += 1;
        block
    };
    if let Some(text) = content.as_str() {
        let text = sanitize(text);
        if role == "user" && !text.is_empty() {
            *user_prompt = true;
            blocks.push(next_block(BlockKind::User, text, None, None));
        } else if !text.is_empty() {
            blocks.push(next_block(BlockKind::Assistant, text, None, None));
        }
        return blocks;
    }
    for part in parts.drain(..) {
        let kind = part["type"].as_str().unwrap_or("");
        match kind {
            "text" | "input_text" | "output_text" => {
                let text = text_of(&part["text"]);
                if text.is_empty() {
                    continue;
                }
                if role == "user" {
                    texts.push(text);
                    *user_prompt = true;
                } else {
                    blocks.push(next_block(BlockKind::Assistant, text, None, None));
                }
            }
            "thinking" | "reasoning" => {
                let text = text_of(
                    part.get("thinking")
                        .or_else(|| part.get("text"))
                        .unwrap_or(&Value::Null),
                );
                if !text.is_empty() {
                    blocks.push(next_block(
                        BlockKind::Assistant,
                        format!("[Thinking]\n{text}"),
                        None,
                        None,
                    ));
                }
            }
            "redacted_thinking" | "encrypted_content" => blocks.push(next_block(
                BlockKind::Notice,
                "[Encrypted reasoning retained by the log]".into(),
                None,
                None,
            )),
            "tool_use" | "server_tool_use" => {
                let Some(call) = string(part, "id") else {
                    warning(warnings, "Tool call lacks a call ID");
                    continue;
                };
                let name = string(part, "name").unwrap_or_else(|| "tool".into());
                let input = part.get("input").or_else(|| part.get("parameters"));
                let text = input
                    .map(|value| sanitize(&serde_json::to_string_pretty(value).unwrap_or_default()))
                    .unwrap_or_default();
                blocks.push(next_block(BlockKind::Tool, text, Some(call), Some(name)));
            }
            "tool_result" => {
                let Some(call) =
                    string(part, "tool_use_id").or_else(|| string(part, "tool_use_id"))
                else {
                    warning(warnings, "Tool result lacks a call ID");
                    continue;
                };
                let text = match part.get("content") {
                    Some(content) if content.is_array() => content
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| {
                            part["text"]
                                .as_str()
                                .map(sanitize)
                                .or_else(|| describe_attachments(part))
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Some(content) => text_of(content),
                    None => String::new(),
                };
                let status = part
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .map(|error| if error { "error" } else { "completed" }.to_string());
                outputs.push(Output {
                    call,
                    text,
                    status,
                    path: path.into(),
                    line,
                });
            }
            _ => {
                if let Some(label) = describe_attachments(part) {
                    texts.push(label);
                } else if !kind.is_empty() {
                    warning(warnings, format!("Unsupported content block {kind:?}"));
                }
            }
        }
    }
    if !texts.is_empty() {
        if role == "user" {
            *user_prompt = true;
        }
        blocks.insert(
            0,
            next_block(
                if role == "user" {
                    BlockKind::User
                } else {
                    BlockKind::Assistant
                },
                texts.join("\n"),
                None,
                None,
            ),
        );
    }
    blocks
}

fn parse_file(
    source: &str,
    path: &Path,
    session: &SessionKey,
    stopped: &AtomicBool,
) -> Result<FileParse> {
    let mut parsed = FileParse {
        ownership_ok: true,
        ..Default::default()
    };
    let mut hasher = blake3::Hasher::new();
    let mut reader = BufReader::new(CheckedRead {
        inner: File::open(path)?,
        stopped,
    });
    let mut line_number = 0u64;
    let mut order = 0usize;
    loop {
        cancel::check(stopped)?;
        let mut bytes = Vec::new();
        let read = reader.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        hasher.update(&bytes);
        let final_line = bytes.last() != Some(&b'\n');
        if final_line {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(_) => {}
                Err(error) if error.is_eof() => {
                    parsed.pending += 1;
                    break;
                }
                Err(_) => {
                    line_number += 1;
                    warning(
                        &mut parsed.warnings,
                        format!("{}:{line_number}: malformed final record", path.display()),
                    );
                    break;
                }
            }
        }
        line_number += 1;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                warning(
                    &mut parsed.warnings,
                    format!(
                        "{}:{line_number}: malformed conversation record",
                        path.display()
                    ),
                );
                continue;
            }
        };
        let kind = value["type"].as_str().unwrap_or("");
        if !matches!(kind, "user" | "assistant") {
            if kind == "system" {
                continue;
            }
            continue;
        }
        // Ownership is checked from recorded metadata, never from message text.
        if session.execution_id.is_none() {
            if let Some(recorded) = string(&value, "sessionId") {
                if recorded != session.native_id {
                    parsed.ownership_ok = false;
                    continue;
                }
            }
        } else if let Some(recorded) = string(&value, "agentId") {
            if session.execution_id.as_deref() != Some(recorded.as_str()) {
                parsed.ownership_ok = false;
                continue;
            }
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        parsed.records += 1;
        if parsed.project.is_none() {
            parsed.project = string(&value, "cwd").map(|cwd| sanitize(&cwd));
        }
        let uuid =
            string(&value, "uuid").unwrap_or_else(|| format!("{}:{line_number}", path.display()));
        let message_id = string(message, "id");
        let model = string(message, "model").filter(|model| model != "<synthetic>");
        let mode = message
            .get("usage")
            .and_then(|usage| usage.get("speed"))
            .and_then(Value::as_str)
            .map(|speed| match speed {
                "standard" => "regular".to_string(),
                other => other.to_string(),
            });
        let mut user_prompt = false;
        let blocks = message_blocks(
            message,
            &uuid,
            timestamp(&value),
            source,
            path,
            line_number,
            model,
            mode,
            &mut parsed.outputs,
            &mut parsed.warnings,
            &mut user_prompt,
        );
        if blocks.is_empty() {
            continue;
        }
        let key = if kind == "assistant" {
            message_id.map(|id| format!("message:{id}")).unwrap_or(uuid)
        } else {
            uuid
        };
        // Fragments of one assistant message merge by message.id.
        if let Some(existing) = parsed.entries.iter_mut().find(|entry| entry.key == key) {
            for block in blocks {
                if !existing
                    .blocks
                    .iter()
                    .any(|current| current.kind == block.kind && current.text == block.text)
                {
                    existing.blocks.push(block);
                }
            }
            existing.timestamp = existing.timestamp.min(timestamp(&value));
        } else {
            parsed.entries.push(Entry {
                key,
                parent: string(&value, "parentUuid"),
                timestamp: timestamp(&value),
                order,
                blocks,
                user_prompt,
            });
            order += 1;
        }
    }
    parsed.hash = *hasher.finalize().as_bytes();
    Ok(parsed)
}

fn discover_files(
    sources: &[Source],
    session: &SessionKey,
    warnings: &mut Vec<String>,
    stopped: &AtomicBool,
) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for source in sources
        .iter()
        .filter(|source| source.agent == AgentKind::ClaudeCode)
    {
        cancel::check(stopped)?;
        let projects = source.root.join("projects");
        if !projects.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&projects).follow_links(false) {
            cancel::check(stopped)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warning(
                        warnings,
                        format!("Cannot inspect {}: {error}", projects.display()),
                    );
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".jsonl"))
            {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let in_subagents = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some("subagents");
            let matches = match &session.execution_id {
                None => !in_subagents && stem == session.native_id,
                Some(execution) => {
                    in_subagents
                        && stem == format!("agent-{execution}")
                        && path
                            .parent()
                            .and_then(Path::parent)
                            .and_then(Path::file_name)
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name == session.native_id)
                }
            };
            if matches {
                files.push((source.name.clone(), path.to_path_buf()));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    files.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    Ok(files)
}

pub(crate) fn load(
    sources: &[Source],
    session: &SessionKey,
    stopped: &AtomicBool,
) -> Result<Conversation> {
    cancel::check(stopped)?;
    if session.native_id.is_empty() {
        bail!("A session ID is required to read its conversation");
    }
    let mut conversation = Conversation {
        session: session.clone(),
        parent: session
            .execution_id
            .as_ref()
            .map(|_| SessionKey::main(AgentKind::ClaudeCode, session.native_id.clone())),
        project: None,
        sources: Vec::new(),
        files: Vec::new(),
        turns: Vec::new(),
        warnings: Vec::new(),
        pending: 0,
    };
    let mut candidates = discover_files(sources, session, &mut conversation.warnings, stopped)?;
    if candidates.is_empty() {
        // Fall back to recorded ownership for renamed transcript files.
        for source in sources
            .iter()
            .filter(|source| source.agent == AgentKind::ClaudeCode)
        {
            let projects = source.root.join("projects");
            if !projects.is_dir() {
                continue;
            }
            for entry in WalkDir::new(&projects).follow_links(false) {
                cancel::check(stopped)?;
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".jsonl"))
                {
                    continue;
                }
                if path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some("subagents")
                {
                    continue;
                }
                // Inspect only the sessionId metadata, not conversation text.
                let Ok(file) = File::open(path) else { continue };
                let mut reader = BufReader::new(file);
                let mut found = false;
                for _ in 0..64 {
                    let mut bytes = Vec::new();
                    if reader.read_until(b'\n', &mut bytes).unwrap_or(0) == 0 {
                        break;
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        if string(&value, "sessionId").as_deref()
                            == Some(session.native_id.as_str())
                        {
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    candidates.push((source.name.clone(), path.to_path_buf()));
                }
            }
        }
        candidates.sort();
        candidates.dedup();
    }
    if candidates.is_empty() {
        bail!(
            "No retained conversation files found for session {} in the selected sources",
            session.selector()
        );
    }

    let mut parses = Vec::new();
    let mut hashes = BTreeSet::new();
    for (source, path) in candidates {
        cancel::check(stopped)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warning(
                    &mut conversation.warnings,
                    format!("Cannot inspect {}: {error}", path.display()),
                );
                continue;
            }
        };
        match parse_file(&source, &path, session, stopped) {
            Ok(parsed) => {
                if !parsed.ownership_ok {
                    warning(
                        &mut conversation.warnings,
                        format!(
                            "{}: records disagree with the selected session/execution and were skipped",
                            path.display()
                        ),
                    );
                }
                if conversation.project.is_none() {
                    conversation.project.clone_from(&parsed.project);
                }
                if !conversation.sources.contains(&source) {
                    conversation.sources.push(source.clone());
                }
                conversation.files.push(ConversationFile {
                    source: source.clone(),
                    path: path.clone(),
                    len: metadata.len(),
                    modified: metadata.modified().ok().map(DateTime::<Utc>::from),
                });
                for message in &parsed.warnings {
                    warning(&mut conversation.warnings, message.clone());
                }
                conversation.pending += parsed.pending;
                // Identical copies are demonstrably duplicates.
                if !hashes.insert(parsed.hash) {
                    continue;
                }
                parses.push(parsed);
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
            session.selector(),
            conversation.warnings.join("; ")
        );
    }

    // Merge copied entries by UUID/message identity; conflicting text is retained.
    let mut entries: Vec<Entry> = Vec::new();
    let mut outputs: Vec<Output> = Vec::new();
    let mut identities: BTreeMap<String, usize> = BTreeMap::new();
    for parsed in parses {
        outputs.extend(parsed.outputs);
        for mut entry in parsed.entries {
            if let Some(&index) = identities.get(&entry.key) {
                let existing = &mut entries[index];
                let same_text = |entry: &Entry, existing: &Entry| {
                    entry.blocks.iter().all(|block| {
                        existing
                            .blocks
                            .iter()
                            .any(|current| current.kind == block.kind && current.text == block.text)
                    })
                };
                if !same_text(&entry, existing) {
                    warning(
                        &mut conversation.warnings,
                        format!(
                            "Conflicting copies of conversation item {}; both retained",
                            entry.key
                        ),
                    );
                    entry.key = format!("{}:conflict:{}", entry.key, entry.order);
                } else {
                    existing.timestamp = existing.timestamp.min(entry.timestamp);
                    existing.parent = existing.parent.clone().or(entry.parent);
                    continue;
                }
            }
            identities.insert(entry.key.clone(), entries.len());
            entries.push(entry);
        }
    }
    // ParentUuid ordering within an execution. Missing parents are notices; cycles
    // fall back to retained file order rather than inventing a branch.
    let known: BTreeSet<String> = entries.iter().map(|entry| entry.key.clone()).collect();
    for entry in &entries {
        if let Some(parent) = &entry.parent {
            if !known.contains(parent) {
                warning(
                    &mut conversation.warnings,
                    format!("Parent conversation item {parent} is not retained"),
                );
            }
        }
    }
    let mut indegree: BTreeMap<&str, usize> = entries
        .iter()
        .map(|entry| (entry.key.as_str(), 0))
        .collect();
    let mut children: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(parent) = entry.parent.as_deref() {
            if indegree.contains_key(parent) && parent != entry.key {
                *indegree.get_mut(entry.key.as_str()).unwrap() += 1;
                children.entry(parent).or_default().push(index);
            }
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|index| (entries[*index].timestamp, entries[*index].key.clone()));
    }
    let mut roots: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| indegree[entry.key.as_str()] == 0)
        .map(|(index, _)| index)
        .collect();
    roots.sort_by_key(|index| (entries[*index].timestamp, entries[*index].key.clone()));
    let mut queue: VecDeque<usize> = roots.into();
    let mut ordered = Vec::with_capacity(entries.len());
    while let Some(index) = queue.pop_front() {
        ordered.push(index);
        for child in children
            .get(entries[index].key.as_str())
            .cloned()
            .unwrap_or_default()
        {
            let degree = indegree.get_mut(entries[child].key.as_str()).unwrap();
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(child);
            }
        }
    }
    if ordered.len() != entries.len() {
        warning(
            &mut conversation.warnings,
            "Conversation parent graph contains a cycle; retained order used for affected items",
        );
        let mut seen = BTreeSet::new();
        ordered.retain(|index| seen.insert(*index));
        for index in 0..entries.len() {
            if seen.insert(index) {
                ordered.push(index);
            }
        }
    }
    let ordered_entries: Vec<Entry> = ordered
        .into_iter()
        .map(|index| entries[index].clone())
        .collect();
    entries = ordered_entries;

    // Pair tool calls and results by call ID within this execution.
    let mut call_index = BTreeMap::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        for (block_index, block) in entry.blocks.iter().enumerate() {
            if let Some(call) = &block.call_id {
                call_index
                    .entry(call.clone())
                    .or_insert((entry_index, block_index));
            }
        }
    }
    let mut seen_outputs = BTreeSet::new();
    let mut orphan_notices = Vec::new();
    for output in outputs {
        cancel::check(stopped)?;
        if !seen_outputs.insert((
            output.call.clone(),
            blake3::hash(output.text.as_bytes()).as_bytes().to_vec(),
        )) {
            continue;
        }
        if let Some(&(entry_index, block_index)) = call_index.get(&output.call) {
            let block = &mut entries[entry_index].blocks[block_index];
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
            orphan_notices.push((
                output.timestamp_order(),
                Block {
                    id: format!("orphan-output:{}:{}", output.path.display(), output.line),
                    kind: BlockKind::Notice,
                    timestamp: None,
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
                    source: String::new(),
                    path: output.path,
                    line: output.line,
                },
            ));
        }
    }
    if let Some(last) = entries.last_mut() {
        last.blocks
            .extend(orphan_notices.into_iter().map(|(_, block)| block));
    } else if !orphan_notices.is_empty() {
        warning(
            &mut conversation.warnings,
            "Tool results without calls were retained as notices",
        );
    }

    // Turns start at user prompts; tool-result-only user envelopes do not.
    let mut current = None;
    for entry in entries {
        if entry.user_prompt {
            let title = entry
                .blocks
                .iter()
                .find(|block| block.kind == BlockKind::User)
                .map(|block| {
                    block
                        .text
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("User message")
                        .chars()
                        .take(160)
                        .collect::<String>()
                })
                .unwrap_or_else(|| "User message".into());
            conversation.turns.push(Turn {
                id: entry.key.clone(),
                timestamp: entry.timestamp,
                title,
                blocks: Vec::new(),
                inherited: false,
            });
            current = Some(conversation.turns.len() - 1);
        }
        if current.is_none() {
            conversation.turns.push(Turn {
                id: "conversation".into(),
                timestamp: entry.timestamp,
                title: "Conversation".into(),
                blocks: Vec::new(),
                inherited: false,
            });
            current = Some(conversation.turns.len() - 1);
        }
        let turn = &mut conversation.turns[current.unwrap()];
        if turn.timestamp.is_none() {
            turn.timestamp = entry.timestamp;
        }
        turn.blocks.extend(entry.blocks);
    }
    if conversation.turns.is_empty() {
        warning(
            &mut conversation.warnings,
            "No readable user or assistant messages are retained in these logs",
        );
    }
    conversation.sources.sort();
    Ok(conversation)
}

impl Output {
    fn timestamp_order(&self) -> usize {
        self.line as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
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
            let mut file = File::create(&path).unwrap();
            for record in records {
                writeln!(file, "{record}").unwrap();
            }
            path
        }
        fn sources(&self) -> Vec<Source> {
            vec![Source {
                name: "claude".into(),
                agent: AgentKind::ClaudeCode,
                kind: "linux".into(),
                root: self.root.clone(),
                enabled: true,
                pricing_provider: None,
            }]
        }
        fn load(&self, session: &SessionKey) -> Conversation {
            load(&self.sources(), session, &AtomicBool::new(false)).unwrap()
        }
    }
    fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
        json!({"type":"user","uuid":uuid,"parentUuid":parent,"sessionId":"session-1",
            "timestamp":"2026-11-01T10:00:00Z","cwd":"/project","message":{"role":"user","content":text}})
    }
    fn assistant(uuid: &str, parent: &str, id: &str, content: Value) -> Value {
        json!({"type":"assistant","uuid":uuid,"parentUuid":parent,"sessionId":"session-1",
            "timestamp":"2026-11-01T10:00:01Z","message":{"id":id,"model":"claude-test","content":content}})
    }
    fn all(c: &Conversation) -> Vec<&Block> {
        c.turns.iter().flat_map(|turn| &turn.blocks).collect()
    }

    #[test]
    fn main_transcript_orders_parent_graph_and_groups_fragments() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[
                user("u1", None, "Question"),
                assistant(
                    "a1",
                    "u1",
                    "msg_1",
                    json!([{"type":"thinking","thinking":"plan"}]),
                ),
                assistant(
                    "a2",
                    "a1",
                    "msg_1",
                    json!([{"type":"text","text":"Answer"},{"type":"tool_use","id":"tool_1","name":"Read","input":{"path":"x"}}]),
                ),
                json!({"type":"user","uuid":"u2","parentUuid":"a2","sessionId":"session-1",
                    "timestamp":"2026-11-01T10:00:02Z",
                    "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"file body"}]}}),
            ],
        );
        let c = fixture.load(&SessionKey::main(AgentKind::ClaudeCode, "session-1"));
        let blocks = all(&c);
        assert_eq!(c.session.native_id, "session-1");
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].text, "Question");
        assert!(blocks[1].text.starts_with("[Thinking]"));
        assert_eq!(blocks[2].text, "Answer");
        assert_eq!(blocks[3].text, "{\n  \"path\": \"x\"\n}");
        assert_eq!(blocks[3].output.as_deref(), Some("file body"));
        // Tool-result-only envelopes do not start a new turn.
        assert_eq!(c.turns.len(), 1);
        assert_eq!(c.turns[0].title, "Question");
        assert_eq!(blocks[2].model.as_deref(), Some("claude-test"));
    }

    #[test]
    fn subagent_transcript_is_separate_and_qualified_selector_loads_it() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[user("u1", None, "Main question")],
        );
        fixture.write(
            "projects/p/session-1/subagents/agent-a.jsonl",
            &[
                json!({"type":"user","uuid":"s1","sessionId":"session-1","agentId":"a",
                    "message":{"role":"user","content":"Sub question"}}),
                json!({"type":"assistant","uuid":"s2","parentUuid":"s1","sessionId":"session-1","agentId":"a",
                    "message":{"id":"m","content":[{"type":"text","text":"Sub answer"}]}}),
            ],
        );
        let main = fixture.load(&SessionKey::main(AgentKind::ClaudeCode, "session-1"));
        assert_eq!(all(&main).len(), 1);
        let sub = fixture.load(&SessionKey::execution(
            AgentKind::ClaudeCode,
            "session-1",
            "a",
        ));
        assert_eq!(
            sub.parent,
            Some(SessionKey::main(AgentKind::ClaudeCode, "session-1"))
        );
        assert_eq!(all(&sub).len(), 2);
        assert_eq!(all(&sub)[1].text, "Sub answer");
    }

    #[test]
    fn copied_records_merge_and_conflicting_text_is_retained_with_warning() {
        let fixture = Fixture::new();
        let records = [user("u1", None, "Question")];
        fixture.write("projects/p/session-1.jsonl", &records);
        fs::create_dir_all(fixture.root.join("projects/q")).unwrap();
        fixture.write("projects/q/session-1.jsonl", &records);
        let c = fixture.load(&SessionKey::main(AgentKind::ClaudeCode, "session-1"));
        assert_eq!(all(&c).len(), 1);
        assert_eq!(c.files.len(), 2);
    }

    #[test]
    fn orphan_results_branches_and_control_characters_are_reported_safely() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[
                user("u1", Some("missing-parent"), "Hi\u{1b}[31m there"),
                json!({"type":"user","uuid":"u2","parentUuid":"u1","sessionId":"session-1",
                    "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"absent","content":"orphan"}]}}),
            ],
        );
        let c = fixture.load(&SessionKey::main(AgentKind::ClaudeCode, "session-1"));
        let text = all(&c)
            .iter()
            .map(|block| block.text.clone())
            .collect::<String>();
        assert!(text.contains("Hi there"));
        assert!(!text.contains('\x1b'));
        assert!(text.contains("Tool result without matching call"));
        assert!(c
            .warnings
            .iter()
            .any(|warning| warning.contains("not retained")));
    }

    #[test]
    fn wrong_session_metadata_is_not_displayed() {
        let fixture = Fixture::new();
        fixture.write(
            "projects/p/session-1.jsonl",
            &[json!({"type":"user","uuid":"u","sessionId":"other",
                "message":{"role":"user","content":"Wrong"}})],
        );
        let conversation = load(
            &fixture.sources(),
            &SessionKey::main(AgentKind::ClaudeCode, "session-1"),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(all(&conversation).is_empty());
        assert!(conversation
            .warnings
            .iter()
            .any(|warning| warning.contains("disagree")));
    }
}
