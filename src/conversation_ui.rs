//! Read-only terminal conversation reader. It never changes accounting or history.
use crate::{
    cli::Cli,
    conversation::{self, Block, BlockKind, Conversation},
    model::Source,
    render::{self, Style},
    report::Report,
};
use anyhow::Result;
use chrono_tz::Tz;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderExit {
    Back,
    Quit,
}

struct Screen;
impl Screen {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
struct CancelReload(Arc<AtomicBool>);
impl Drop for CancelReload {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct Line {
    text: String,
    role: &'static str,
    turn: usize,
    block: Option<String>,
    source: Option<(Part, usize)>,
}
#[derive(Default)]
struct Document {
    lines: Vec<Line>,
    focus: Vec<(String, usize)>,
    turns: Vec<(usize, usize)>,
    folded: BTreeSet<String>,
}
#[derive(Default)]
struct State {
    offset: usize,
    focused: Option<String>,
    expanded: BTreeSet<String>,
    limits: BTreeMap<String, usize>,
    windows: BTreeMap<String, (Part, usize)>,
    saved_offset: usize,
    saved_anchor: Option<ReadingAnchor>,
    tools: bool,
    inherited: bool,
    details: bool,
    help: bool,
    query: String,
    search_input: Option<String>,
    hits: Vec<SearchHit>,
    hits_limited: bool,
    searching: bool,
    hit: usize,
    selected_turn: usize,
    notice: Option<String>,
    outline_offset: usize,
    model_hits: Vec<ModelHit>,
    model_focus: String,
}

fn timezone(report: &Report) -> Tz {
    report.period.timezone.parse().unwrap_or(chrono_tz::UTC)
}
fn clock(time: Option<chrono::DateTime<chrono::Utc>>, tz: Tz) -> String {
    time.map(|time| time.with_timezone(&tz).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}
fn short(value: &str, width: usize) -> String {
    let clean = conversation::sanitize(value).replace(['\n', '\r'], " ");
    if clean.width() <= width {
        return clean;
    }
    let mut s = String::new();
    for c in clean.chars() {
        if s.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 3 > width {
            break;
        }
        s.push(c);
    }
    if width >= 3 {
        s.push_str("...");
    }
    s
}
fn key(block: &Block) -> String {
    block.id.clone()
}
fn heading(block: &Block, tz: Tz, ascii: bool) -> String {
    let sep = if ascii { " / " } else { " · " };
    let label = match block.kind {
        BlockKind::User => "You",
        BlockKind::Assistant => "Assistant",
        BlockKind::Tool => "Tool",
        BlockKind::Notice => "Notice",
    };
    let mut bits = vec![format!("{}  {label}", clock(block.timestamp, tz))];
    if let Some(name) = &block.tool_name {
        bits.push(conversation::sanitize(name));
    }
    if matches!(block.kind, BlockKind::Assistant | BlockKind::Tool) {
        if let Some(model) = &block.model {
            bits.push(conversation::sanitize(model));
        }
        if let Some(mode) = &block.mode {
            let mut label = mode.clone();
            if let Some(first) = label.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            bits.push(conversation::sanitize(&label));
        }
    }
    if let Some(status) = &block.status {
        bits.push(conversation::sanitize(status));
    }
    if let Some(ms) = block.duration_ms {
        bits.push(format!("{:.1}s", ms as f64 / 1000.0));
    }
    bits.join(sep)
}
fn role(block: &Block) -> &'static str {
    match block.kind {
        BlockKind::User => "accent",
        BlockKind::Assistant => "bold",
        BlockKind::Tool => "meta",
        BlockKind::Notice => "warn",
    }
}
fn full_text(block: &Block) -> String {
    let mut text = conversation::sanitize(&block.text);
    if let Some(output) = &block.output {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("Result:\n");
        text.push_str(&conversation::sanitize(output));
    }
    text
}
fn append(
    doc: &mut Document,
    text: &str,
    style: &'static str,
    width: usize,
    turn: usize,
    id: Option<&str>,
) {
    for line in render::wrap(text, width.max(1)) {
        doc.lines.push(Line {
            text: line,
            role: style,
            turn,
            block: id.map(str::to_owned),
            source: None,
        });
    }
}
/// Wrap only a bounded display window, including enormous single-line results.
fn body(
    doc: &mut Document,
    block: &Block,
    width: usize,
    turn: usize,
    state: &State,
    stopped: &AtomicBool,
) -> Result<()> {
    let id = key(block);
    let expanded = state.expanded.contains(&id);
    let limit = if expanded {
        state.limits.get(&id).copied().unwrap_or(2000)
    } else {
        20
    };
    let window = state.windows.get(&id).copied();
    let mut pieces: Vec<(Part, &str, &str)> = Vec::new();
    if window.is_some_and(|(part, _)| part == Part::Output) {
        pieces.push((
            Part::Output,
            "Result:",
            block.output.as_deref().unwrap_or(""),
        ));
    } else {
        pieces.push((Part::Text, "", &block.text));
        if let Some(output) = &block.output {
            pieces.push((Part::Output, "Result:", output));
        }
    }
    if window.is_some_and(|(_, start)| start > 0)
        || window.is_some_and(|(part, _)| part == Part::Output)
    {
        append(
            doc,
            "... earlier retained content / Backspace collapse",
            "meta",
            width,
            turn,
            Some(&id),
        );
    }
    let mut shown = 0;
    let mut folded = false;
    let mut any = false;
    for (piece_index, (part, label, text)) in pieces.iter().enumerate() {
        if !label.is_empty() {
            append(doc, label, "meta", width, turn, Some(&id));
        }
        let start = if piece_index == 0 {
            window.map(|(_, start)| start).unwrap_or(0)
        } else {
            0
        };
        let start = start.min(text.len());
        let mut chars = text[start..].char_indices().peekable();
        let mut line_start = start;
        let mut line = String::new();
        let mut cells = 0usize;
        let mut visited = 0usize;
        let char_budget = limit
            .saturating_mul(width.max(1))
            .saturating_mul(8)
            .saturating_add(4096);
        while let Some((byte, c)) = chars.next() {
            let source_byte = start + byte;
            let source_end = source_byte + c.len_utf8();
            visited += 1;
            if visited.is_multiple_of(1024) {
                crate::cancel::check(stopped)?;
            }
            if visited > char_budget {
                folded = true;
                break;
            }
            let letters: &str = if c == '\t' { "    " } else { "" };
            let expanded_char = if letters.is_empty() {
                c.to_string()
            } else {
                letters.into()
            };
            for c in expanded_char.chars() {
                if c == '\n'
                    || (cells + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
                        > width.max(1)
                        && !line.is_empty())
                {
                    doc.lines.push(Line {
                        text: std::mem::take(&mut line),
                        role: if block.kind == BlockKind::Tool {
                            "meta"
                        } else {
                            "body"
                        },
                        turn,
                        block: Some(id.clone()),
                        source: Some((*part, line_start)),
                    });
                    line_start = if c == '\n' { source_end } else { source_byte };
                    shown += 1;
                    any = true;
                    cells = 0;
                    if shown >= limit {
                        folded =
                            c != '\n' || chars.peek().is_some() || piece_index + 1 < pieces.len();
                        break;
                    }
                }
                if c != '\n' && !c.is_control() {
                    cells += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    line.push(c);
                }
            }
            if shown >= limit || folded {
                break;
            }
        }
        if !line.is_empty() && shown < limit {
            doc.lines.push(Line {
                text: line,
                role: if block.kind == BlockKind::Tool {
                    "meta"
                } else {
                    "body"
                },
                turn,
                block: Some(id.clone()),
                source: Some((*part, line_start)),
            });
            shown += 1;
            any = true;
        }
        if shown >= limit && piece_index + 1 < pieces.len() {
            folded = true;
        }
        if folded {
            break;
        }
    }
    if !any {
        append(
            doc,
            "[Message text unavailable in this log]",
            "warn",
            width,
            turn,
            Some(&id),
        );
    }
    if folded {
        doc.folded.insert(id.clone());
        append(
            doc,
            "... more retained content / Enter show more",
            "meta",
            width,
            turn,
            Some(&id),
        );
    }
    Ok(())
}
fn document(
    conversation: &Conversation,
    state: &State,
    width: usize,
    tz: Tz,
    ascii: bool,
    stopped: &AtomicBool,
) -> Result<Document> {
    let mut doc = Document::default();
    let mut earlier = false;
    for (turn_index, turn) in conversation.turns.iter().enumerate() {
        crate::cancel::check(stopped)?;
        if turn.inherited {
            if !earlier {
                doc.focus
                    .push(("__earlier_context".into(), doc.lines.len()));
                append(
                    &mut doc,
                    if state.inherited {
                        if ascii {
                            "v Earlier context / Enter collapse"
                        } else {
                            "▾ Earlier context · Enter collapse"
                        }
                    } else if ascii {
                        "> Earlier context / Enter expand"
                    } else {
                        "▸ Earlier context · Enter expand"
                    },
                    "meta",
                    width,
                    turn_index,
                    Some("__earlier_context"),
                );
                doc.lines.push(Line {
                    text: String::new(),
                    role: "body",
                    turn: turn_index,
                    block: None,
                    source: None,
                });
                earlier = true;
            }
            if !state.inherited {
                continue;
            }
        }
        doc.turns.push((turn_index, doc.lines.len()));
        append(
            &mut doc,
            &format!("Turn {} / {}", turn_index + 1, conversation.turns.len()),
            "meta",
            width,
            turn_index,
            None,
        );
        let mut i = 0;
        while i < turn.blocks.len() {
            let block = &turn.blocks[i];
            if block.kind == BlockKind::Tool && !state.tools {
                let end = turn.blocks[i..]
                    .iter()
                    .position(|b| b.kind != BlockKind::Tool)
                    .map(|n| i + n)
                    .unwrap_or(turn.blocks.len());
                let id = format!("tools:{}", block.id);
                doc.focus.push((id.clone(), doc.lines.len()));
                let prefix = if ascii { ">" } else { "▸" };
                append(
                    &mut doc,
                    &format!("{prefix} {} tool calls / Enter expand", end - i),
                    "meta",
                    width,
                    turn_index,
                    Some(&id),
                );
                doc.lines.push(Line {
                    text: String::new(),
                    role: "body",
                    turn: turn_index,
                    block: None,
                    source: None,
                });
                i = end;
                continue;
            }
            let id = key(block);
            doc.focus.push((id.clone(), doc.lines.len()));
            let expanded = state.expanded.contains(&id);
            let mut title = heading(block, tz, ascii);
            if block.kind == BlockKind::Tool {
                title = format!(
                    "{} {title}",
                    if expanded {
                        if ascii {
                            "v"
                        } else {
                            "▾"
                        }
                    } else if ascii {
                        ">"
                    } else {
                        "▸"
                    }
                );
            }
            if width < 58 {
                let sep = if ascii { " / " } else { " · " };
                let mut parts = title.splitn(2, sep);
                append(
                    &mut doc,
                    parts.next().unwrap_or(""),
                    role(block),
                    width,
                    turn_index,
                    Some(&id),
                );
                if let Some(metadata) = parts.next() {
                    append(&mut doc, metadata, "meta", width, turn_index, Some(&id));
                }
            } else {
                append(&mut doc, &title, role(block), width, turn_index, Some(&id));
            }
            if block.kind != BlockKind::Tool || expanded {
                body(&mut doc, block, width, turn_index, state, stopped)?;
            }
            doc.lines.push(Line {
                text: String::new(),
                role: "body",
                turn: turn_index,
                block: None,
                source: None,
            });
            i += 1;
        }
    }
    if doc.lines.is_empty() {
        append(
            &mut doc,
            "No retained conversation messages.",
            "meta",
            width,
            0,
            None,
        );
    }
    Ok(doc)
}

fn initial_turn(conversation: &Conversation, report: &Report, filtered: bool) -> usize {
    conversation
        .turns
        .iter()
        .enumerate()
        .find(|(_, turn)| {
            filtered
                && (report.period.start.is_some() || report.period.end.is_some())
                && !turn.inherited
                && turn.timestamp.is_some_and(|t| report.period.includes(t))
        })
        .map(|(i, _)| i)
        .or_else(|| conversation.turns.iter().rposition(|turn| !turn.inherited))
        .unwrap_or(0)
}
fn locate_turn(doc: &Document, turn: usize) -> usize {
    doc.turns
        .iter()
        .find(|(index, _)| *index == turn)
        .map(|(_, line)| *line)
        .unwrap_or(0)
}
fn locate_block(doc: &Document, id: &str) -> Option<usize> {
    doc.focus
        .iter()
        .find(|(key, _)| key == id)
        .map(|(_, line)| *line)
        .or_else(|| {
            doc.lines
                .iter()
                .position(|line| line.block.as_deref() == Some(id))
        })
}
fn top_block(doc: &Document, offset: usize) -> Option<String> {
    doc.lines
        .get(offset)
        .and_then(|line| line.block.clone())
        .or_else(|| {
            doc.lines
                .iter()
                .skip(offset)
                .find_map(|line| line.block.clone())
        })
}
#[derive(Debug, Clone)]
struct ReadingAnchor {
    block: Option<String>,
    source: Option<(Part, usize)>,
    block_row: usize,
    turn: usize,
    turn_row: usize,
}
fn reading_anchor(doc: &Document, offset: usize) -> Option<ReadingAnchor> {
    let line = doc.lines.get(offset)?;
    let start = line
        .block
        .as_deref()
        .and_then(|id| locate_block(doc, id))
        .unwrap_or(offset);
    Some(ReadingAnchor {
        block: line.block.clone(),
        source: line.source,
        block_row: offset.saturating_sub(start),
        turn: line.turn,
        turn_row: offset.saturating_sub(locate_turn(doc, line.turn)),
    })
}
fn restore_anchor(doc: &Document, anchor: &ReadingAnchor) -> usize {
    if let Some(id) = &anchor.block {
        if let Some((part, byte)) = anchor.source {
            if let Some(index) = doc.lines.iter().rposition(|line| {
                line.block.as_ref() == Some(id)
                    && line
                        .source
                        .is_some_and(|(p, start)| p == part && start <= byte)
            }) {
                return index;
            }
        }
        if let Some(start) = locate_block(doc, id) {
            let length = doc
                .lines
                .iter()
                .skip(start)
                .take_while(|line| line.block.as_ref() == Some(id))
                .count();
            return start + anchor.block_row.min(length.saturating_sub(1));
        }
    }
    let start = locate_turn(doc, anchor.turn);
    let length = doc
        .lines
        .iter()
        .skip(start)
        .take_while(|line| line.turn == anchor.turn)
        .count();
    start + anchor.turn_row.min(length.saturating_sub(1))
}
fn block_visible(doc: &Document, id: &str, offset: usize, page: usize) -> bool {
    doc.lines
        .iter()
        .skip(offset)
        .take(page)
        .any(|line| line.block.as_deref() == Some(id))
}
fn focus_next(state: &mut State, doc: &Document, reverse: bool, page: usize) {
    if doc.focus.is_empty() {
        return;
    }
    let current = state
        .focused
        .as_ref()
        .filter(|id| block_visible(doc, id, state.offset, page))
        .and_then(|id| doc.focus.iter().position(|(key, _)| key == id));
    let index = match current {
        Some(i) if reverse => i.saturating_sub(1),
        Some(i) => (i + 1).min(doc.focus.len() - 1),
        None if reverse => doc
            .focus
            .iter()
            .rposition(|(_, line)| *line < state.offset + page)
            .unwrap_or(0),
        None => top_block(doc, state.offset)
            .and_then(|id| doc.focus.iter().position(|(key, _)| key == &id))
            .or_else(|| doc.focus.iter().position(|(_, line)| *line >= state.offset))
            .unwrap_or(doc.focus.len() - 1),
    };
    let (id, line) = &doc.focus[index];
    state.focused = Some(id.clone());
    if !block_visible(doc, id, state.offset, page) {
        state.offset = if *line < state.offset {
            *line
        } else {
            (line + 1).saturating_sub(page)
        };
    }
}
#[derive(Clone, Copy)]
enum Scroll {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}
fn scroll(state: &mut State, content_len: usize, page: usize, action: Scroll) {
    let end = content_len.saturating_sub(page);
    state.offset = match action {
        Scroll::Up => state.offset.saturating_sub(1),
        Scroll::Down => state.offset.saturating_add(1),
        Scroll::PageUp => state.offset.saturating_sub(page),
        Scroll::PageDown => state.offset.saturating_add(page),
        Scroll::Home => 0,
        Scroll::End => end,
    }
    .min(end);
    state.focused = None;
}
#[derive(Clone)]
struct ModelHit {
    turn: usize,
    block: String,
}
fn model_hits(data: &Conversation, cli: &Cli, report: &Report) -> Vec<ModelHit> {
    if cli.models.is_empty() && cli.mode.is_none() {
        return Vec::new();
    }
    data.turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| !turn.inherited)
        .filter_map(|(turn, value)| {
            value
                .blocks
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Assistant | BlockKind::Tool))
                .find(|block| {
                    (cli.models.is_empty()
                        || block
                            .model
                            .as_ref()
                            .is_some_and(|model| cli.models.contains(model)))
                        && cli.mode.is_none_or(|mode| {
                            block.mode.as_deref().unwrap_or("unknown") == mode.as_str()
                        })
                        && block
                            .timestamp
                            .or(value.timestamp)
                            .is_none_or(|time| report.period.includes(time))
                })
                .map(|block| ModelHit {
                    turn,
                    block: key(block),
                })
        })
        .collect()
}
fn jump_model(state: &mut State, reverse: bool) -> Option<usize> {
    let hit = if reverse {
        state
            .model_hits
            .iter()
            .rfind(|hit| hit.turn < state.selected_turn)
            .or_else(|| state.model_hits.last())
    } else {
        state
            .model_hits
            .iter()
            .find(|hit| hit.turn > state.selected_turn)
            .or_else(|| state.model_hits.first())
    }?
    .clone();
    state.focused = Some(hit.block);
    state.selected_turn = hit.turn;
    Some(hit.turn)
}
fn reveal_model(data: &Conversation, state: &mut State, hit: &ModelHit) {
    state.focused = Some(hit.block.clone());
    state.selected_turn = hit.turn;
    if data.turns.get(hit.turn).is_some_and(|turn| {
        turn.blocks
            .iter()
            .any(|block| key(block) == hit.block && block.kind == BlockKind::Tool)
    }) {
        state.tools = true;
    }
}
fn model_offset(doc: &Document, hit: &ModelHit, page: usize) -> usize {
    let turn = locate_turn(doc, hit.turn);
    let block = locate_block(doc, &hit.block).unwrap_or(turn);
    // Keep the prompt when it fits, otherwise show the invocation with nearby context.
    if block.saturating_sub(turn) < page.saturating_sub(3) {
        turn
    } else {
        block.saturating_sub(page / 3).max(turn)
    }
}
fn keep_turn_visible(state: &mut State, page: usize) {
    let rows = page.saturating_sub(1).max(1);
    if state.selected_turn < state.outline_offset {
        state.outline_offset = state.selected_turn;
    } else if state.selected_turn >= state.outline_offset + rows {
        state.outline_offset = state.selected_turn + 1 - rows;
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Heading,
    Text,
    Output,
}
#[derive(Debug, Clone)]
struct SearchHit {
    turn: usize,
    block: usize,
    part: Part,
    byte: usize,
}
struct SearchRequest {
    data: Arc<Conversation>,
    query: String,
    cancelled: Arc<AtomicBool>,
    generation: u64,
}
struct SearchResult {
    generation: u64,
    hits: Vec<SearchHit>,
    limited: bool,
}
#[derive(Default)]
struct SearchCancel(Option<Arc<AtomicBool>>);
impl Drop for SearchCancel {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.store(true, Ordering::Relaxed);
        }
    }
}
fn search(
    data: &Conversation,
    query: &str,
    cancelled: &AtomicBool,
) -> Result<(Vec<SearchHit>, bool)> {
    let pattern: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if pattern.is_empty() {
        return Ok((Vec::new(), false));
    }
    let mut prefix = vec![0; pattern.len()];
    for i in 1..pattern.len() {
        let mut j = prefix[i - 1];
        while j > 0 && pattern[i] != pattern[j] {
            j = prefix[j - 1];
        }
        if pattern[i] == pattern[j] {
            j += 1;
        }
        prefix[i] = j;
    }
    let mut hits = Vec::new();
    for (turn, value) in data.turns.iter().enumerate() {
        for (block, item) in value.blocks.iter().enumerate() {
            let heading = heading(item, chrono_tz::UTC, true);
            let metadata = heading
                .split_once("  ")
                .map(|(_, text)| text)
                .unwrap_or(&heading);
            for (part, text) in [
                (Part::Heading, metadata),
                (Part::Text, item.text.as_str()),
                (Part::Output, item.output.as_deref().unwrap_or("")),
            ] {
                crate::cancel::check(cancelled)?;
                let mut matched = 0;
                let mut positions = VecDeque::with_capacity(pattern.len());
                for (visited, (byte, c)) in text.char_indices().enumerate() {
                    if visited.is_multiple_of(4096) {
                        crate::cancel::check(cancelled)?;
                    }
                    for c in c.to_lowercase() {
                        positions.push_back(byte);
                        if positions.len() > pattern.len() {
                            positions.pop_front();
                        }
                        while matched > 0 && c != pattern[matched] {
                            matched = prefix[matched - 1];
                        }
                        if c == pattern[matched] {
                            matched += 1;
                        }
                        if matched == pattern.len() {
                            hits.push(SearchHit {
                                turn,
                                block,
                                part,
                                byte: *positions.front().unwrap(),
                            });
                            if hits.len() == 20_000 {
                                return Ok((hits, true));
                            }
                            matched = prefix[matched - 1];
                        }
                    }
                }
            }
        }
    }
    Ok((hits, false))
}
fn reveal_hit(conversation: &Conversation, state: &mut State) -> Option<String> {
    let hit = state.hits.get(state.hit)?;
    let turn = conversation.turns.get(hit.turn)?;
    let block = turn.blocks.get(hit.block)?;
    let id = key(block);
    state.expanded.insert(id.clone());
    state.limits.insert(id.clone(), 2000);
    if hit.part == Part::Heading {
        state.windows.remove(&id);
    } else {
        let text = match hit.part {
            Part::Text => block.text.as_str(),
            Part::Output => block.output.as_deref().unwrap_or(""),
            Part::Heading => unreachable!(),
        };
        let mut start = hit.byte.saturating_sub(120);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        if let Some(newline) = text[start..hit.byte].rfind('\n') {
            start += newline + 1;
        }
        state.windows.insert(id.clone(), (hit.part, start));
    }
    if turn.inherited {
        state.inherited = true;
    }
    if block.kind == BlockKind::Tool {
        state.tools = true;
    }
    state.focused = Some(id.clone());
    state.selected_turn = hit.turn;
    Some(id)
}
fn hit_line(doc: &Document, state: &State, id: &str) -> Option<usize> {
    let query = state.query.to_lowercase();
    doc.lines.iter().position(|line| {
        line.block.as_deref() == Some(id) && line.text.to_lowercase().contains(&query)
    })
}
fn paint_line(style: &Style, line: &Line, query: &str, focused: bool) -> String {
    let text = if focused {
        format!("> {}", line.text)
    } else {
        line.text.clone()
    };
    // Match offsets use lowercased substrings only when byte lengths agree.
    // Otherwise highlight the whole matching line rather than slicing Unicode unsafely.
    if !query.is_empty() && text.to_lowercase().contains(&query.to_lowercase()) {
        style.paint("accent", text)
    } else {
        style.paint(line.role, text)
    }
}
fn details(conversation: &Conversation, report: &Report, width: usize) -> Vec<String> {
    let mut raw = vec![
        format!("Session: {}", conversation.session_id),
        format!(
            "Project: {}",
            conversation.project.as_deref().unwrap_or("unknown")
        ),
        format!("Sources: {}", conversation.sources.join(", ")),
        format!(
            "Conversation: all retained messages / {} turns",
            conversation.turns.len()
        ),
        format!(
            "Report period: {} ({})",
            report.period.label, report.period.timezone
        ),
        format!(
            "Usage snapshot: {}",
            report
                .generated_at
                .with_timezone(&timezone(report))
                .format("%Y-%m-%d %H:%M:%S %Z")
        ),
    ];
    let counts: Vec<_> = if report.kind == "Session" {
        vec![&report.totals]
    } else {
        report
            .rows
            .iter()
            .filter(|row| row.dimensions.get("session") == Some(&conversation.session_id))
            .map(|row| &row.counts)
            .collect()
    };
    let tokens = counts
        .iter()
        .fold(0u64, |sum, c| sum.saturating_add(c.total));
    let known: Vec<_> = counts.iter().filter_map(|c| c.cost_usd).collect();
    let cost = if known.is_empty() {
        None
    } else {
        known
            .into_iter()
            .try_fold(rust_decimal::Decimal::ZERO, |sum, c| sum.checked_add(c))
    };
    let partial = counts
        .iter()
        .any(|c| c.unpriced_tokens > 0 || c.assumed_events > 0);
    raw.push(format!(
        "Selected-period usage: {} tokens / {}{}",
        render::integer(tokens),
        render::money(cost),
        if partial { "*" } else { "" }
    ));
    if partial {
        raw.push("* Known priced subtotal; some usage is unpriced or assumed.".into());
    }
    if let Some(parent) = &conversation.parent_session {
        raw.push(format!("Parent session: {parent}"));
    }
    if conversation.pending > 0 {
        raw.push(format!("Pending records: {}", conversation.pending));
    }
    raw.extend(conversation.warnings.iter().map(|s| format!("Data: {s}")));
    for file in &conversation.files {
        raw.push(format!("{}: {}", file.source, file.path.display()));
    }
    raw.into_iter()
        .flat_map(|line| render::wrap(&conversation::sanitize(&line), width))
        .collect()
}
fn footer(state: &State, turns: usize, width: usize, updated: bool, loading: bool) -> Vec<String> {
    if let Some(input) = &state.search_input {
        return vec![short(
            &format!("/{input}_   Enter Search   Esc Cancel"),
            width,
        )];
    }
    let status = if state.searching {
        " / Searching..."
    } else if loading {
        " / Loading..."
    } else if updated {
        " / Updated on disk: r Refresh"
    } else {
        ""
    };
    let query = if state.query.is_empty() {
        String::new()
    } else {
        format!(
            " / {}/{}{} matches",
            if state.hits.is_empty() {
                0
            } else {
                state.hit + 1
            },
            state.hits.len(),
            if state.hits_limited { "+" } else { "" }
        )
    };
    let digits = turns.max(1).to_string().len();
    let model_navigation = if state.model_hits.is_empty() {
        ""
    } else {
        "  m/M Model"
    };
    let line = if width < 60 {
        format!(
            "{:>digits$}/{}  t Tools  / Find  Esc Back{model_navigation}{status}{query}",
            state.selected_turn + 1,
            turns.max(1)
        )
    } else {
        format!(
            "Turn {:>digits$}/{}  [ ] Turn  t Tools  / Search  Esc Back  ? Help{model_navigation}{status}{query}",
            state.selected_turn + 1,
            turns.max(1)
        )
    };
    render::wrap(&line, width)
}
struct Paint<'a> {
    cli: &'a Cli,
    report: &'a Report,
    style: &'a Style,
    width: usize,
    height: usize,
    updated: bool,
    loading: bool,
}
fn help(width: usize) -> Vec<String> {
    ["Conversation help", "j/k or arrows scroll; Page Up/Down and Home/End move through the conversation.","[ and ] jump turns. m/M jump to next/previous selected-model turn. Tab/Shift+Tab focus visible blocks; Enter expands/collapses.","Long blocks load in 2,000-line pages with Enter. Backspace collapses the focused block.","t shows/hides tool calls. / searches message/tool text and model/mode labels; n/N find next/previous matches.","i shows session/source/report details. r reloads changed logs without altering usage accounting.","Esc clears search or closes details/help before returning. q quits; Ctrl+C also cancels loading.","Earlier context is collapsed by default. Plain exports retain full text."].into_iter().flat_map(|line|render::wrap(line,width)).collect()
}
struct Chrome {
    header: Vec<String>,
    footer: Vec<String>,
    page: usize,
}
fn chrome(conversation: &Conversation, state: &State, paint: &Paint<'_>) -> Chrome {
    let cli = paint.cli;
    let report = paint.report;
    let width = paint.width;
    let height = paint.height;
    let updated = paint.updated;
    let loading = paint.loading;
    let sep = if cli.ascii { " / " } else { " · " };
    let mut header = vec![short(
        &format!(
            "Codex Usage / Sessions / {}{}{}",
            short(&conversation.session_id, 20),
            sep,
            conversation.project.as_deref().unwrap_or("unknown project")
        ),
        width,
    )];
    let date = conversation
        .turns
        .get(state.selected_turn)
        .and_then(|turn| turn.timestamp)
        .map(|time| {
            time.with_timezone(&timezone(report))
                .format("%b %d %Y")
                .to_string()
        })
        .unwrap_or_else(|| "Date unavailable".into());
    let mut context = if width < 60 {
        format!("All retained conversation{sep}{date}")
    } else {
        format!(
            "{date}{sep}{}{sep}All retained conversation{sep}{}",
            report.period.timezone,
            conversation.sources.join(" + ")
        )
    };
    if !state.model_focus.is_empty() {
        context = format!(
            "{date}{sep}{}{sep}All retained conversation{sep}Focus {} ({} turns)",
            report.period.timezone,
            state.model_focus,
            state.model_hits.len()
        );
    }
    if !conversation.warnings.is_empty() || conversation.pending > 0 {
        context = format!("partial data{sep}{context}");
    }
    header.push(short(&context, width));
    header.push(String::new());
    let mut footer = footer(state, conversation.turns.len(), width, updated, loading);
    if header.len() + footer.len() >= height {
        footer = render::wrap("Esc Back / q Quit", width);
    }
    let page = height.saturating_sub(header.len() + footer.len()).max(1);
    Chrome {
        header,
        footer,
        page,
    }
}
fn frame(
    conversation: &Conversation,
    state: &State,
    doc: &Document,
    paint: &Paint<'_>,
) -> Vec<String> {
    let Paint {
        report,
        style,
        width,
        height,
        ..
    } = *paint;
    if width < 24 || height < 7 {
        return render::wrap("Resize terminal / Esc Back / q Quit", width)
            .into_iter()
            .take(height)
            .map(|s| style.paint("meta", s))
            .collect();
    }
    let Chrome {
        header,
        footer,
        page,
    } = chrome(conversation, state, paint);
    let mut out: Vec<String> = header
        .into_iter()
        .map(|s| style.paint("meta", conversation::sanitize(&s)))
        .collect();
    if state.help || state.details {
        let lines = if state.details {
            details(conversation, report, width)
        } else {
            help(width)
        };
        out.extend(
            lines
                .into_iter()
                .skip(state.offset)
                .take(page)
                .map(|s| style.paint("meta", s)),
        );
    } else {
        let current_line = state.focused.as_deref().and_then(|id| {
            doc.lines
                .iter()
                .enumerate()
                .skip(state.offset)
                .take(page)
                .find(|(_, line)| line.block.as_deref() == Some(id))
                .map(|(index, _)| index)
        });
        let outline_start = state.outline_offset;
        for index in 0..page {
            let row = doc.lines.get(state.offset + index);
            if width >= 120 {
                let outline = if index == 0 {
                    "Turns".to_string()
                } else if let Some(turn) = conversation.turns.get(outline_start + index - 1) {
                    format!(
                        "{} {:02} {} {}",
                        if outline_start + index - 1 == state.selected_turn {
                            ">"
                        } else {
                            " "
                        },
                        outline_start + index,
                        clock(turn.timestamp, timezone(report)),
                        turn.title
                    )
                } else {
                    String::new()
                };
                let outline = short(&outline, 28);
                let padding = 30usize.saturating_sub(outline.width());
                let mut line = style.paint(
                    if outline_start + index.saturating_sub(1) == state.selected_turn {
                        "accent"
                    } else {
                        "meta"
                    },
                    outline,
                );
                line.push_str(&" ".repeat(padding));
                if let Some(row) = row {
                    line.push_str(&paint_line(
                        style,
                        row,
                        &state.query,
                        current_line == Some(state.offset + index),
                    ));
                }
                out.push(line);
            } else if let Some(row) = row {
                out.push(paint_line(
                    style,
                    row,
                    &state.query,
                    current_line == Some(state.offset + index),
                ));
            } else {
                out.push(String::new());
            }
        }
    }
    while out.len() < height.saturating_sub(footer.len()) {
        out.push(String::new());
    }
    out.extend(footer.into_iter().map(|s| style.paint("meta", s)));
    out.truncate(height);
    out
}

/// Full plain-text transcript for pipes and pager-free output; never applies previews.
pub fn plain(conversation: &Conversation, tz: Tz) -> String {
    let mut out = format!(
        "Session {}\nProject: {}\nAll retained conversation / {}\n\n",
        conversation::sanitize(&conversation.session_id),
        conversation::sanitize(conversation.project.as_deref().unwrap_or("unknown")),
        tz
    );
    for turn in &conversation.turns {
        out.push_str(&format!(
            "{}Turn {} / {}\n",
            if turn.inherited {
                "Earlier context / "
            } else {
                ""
            },
            conversation::sanitize(&turn.id),
            clock(turn.timestamp, tz)
        ));
        for block in &turn.blocks {
            out.push_str(&heading(block, tz, true));
            out.push('\n');
            out.push_str(&full_text(block));
            out.push_str("\n\n");
        }
    }
    if conversation.pending > 0 {
        out.push_str(&format!(
            "[{} incomplete records pending]\n",
            conversation.pending
        ));
    }
    for warning in &conversation.warnings {
        out.push_str(&format!("[{}]\n", conversation::sanitize(warning)));
    }
    out
}

/// Open the reader. A nested report owns the terminal when `manage_screen` is false.
pub fn run(
    cli: &Cli,
    conversation: Conversation,
    report: &Report,
    sources: &[Source],
    stopped: Arc<AtomicBool>,
    manage_screen: bool,
) -> Result<ReaderExit> {
    let _screen = if manage_screen {
        Some(Screen::enter()?)
    } else {
        None
    };
    let mut conversation = Arc::new(conversation);
    let style = Style::new(cli, true);
    let tz = timezone(report);
    let matching = model_hits(&conversation, cli, report);
    let selected_turn = matching.first().map(|hit| hit.turn).unwrap_or_else(|| {
        initial_turn(
            &conversation,
            report,
            !manage_screen
                || cli.today
                || cli.last.is_some()
                || cli.since.is_some()
                || cli.until.is_some(),
        )
    });
    let mut target = cli.models.clone();
    if let Some(mode) = cli.mode {
        target.push(format!("{mode:?}"));
    }
    let mut state = State {
        selected_turn,
        focused: matching.first().map(|hit| hit.block.clone()),
        model_hits: matching,
        model_focus: target.join(" / "),
        ..Default::default()
    };
    let initial_model = state.model_hits.first().cloned();
    if let Some(hit) = &initial_model {
        reveal_model(&conversation, &mut state, hit);
    }
    std::thread::scope(|scope| -> Result<ReaderExit> {
        let (requests_tx, requests_rx) = std::sync::mpsc::channel::<()>();
        let (results_tx, results_rx) = std::sync::mpsc::channel();
        let reload_stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&reload_stop);
        let _cancel = CancelReload(Arc::clone(&reload_stop));
        let session = conversation.session_id.clone();
        scope.spawn(move || {
            while requests_rx.recv().is_ok() {
                let result = conversation::load(sources, &session, &worker_stop);
                if results_tx.send(result).is_err() {
                    break;
                }
            }
        });
        let (search_tx, search_rx) = std::sync::mpsc::channel::<SearchRequest>();
        let (found_tx, found_rx) = std::sync::mpsc::channel::<SearchResult>();
        scope.spawn(move || {
            while let Ok(job) = search_rx.recv() {
                if let Ok((hits, limited)) = search(&job.data, &job.query, &job.cancelled) {
                    if found_tx
                        .send(SearchResult {
                            generation: job.generation,
                            hits,
                            limited,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let mut search_cancel = SearchCancel::default();
        let mut search_generation = 0u64;
        let mut loading = false;
        let mut dirty = true;
        let mut rebuild = true;
        let mut old_width = 0;
        let mut old_height = 0;
        let mut document_width = 0;
        let mut doc = Document::default();
        let mut jump_turn = if initial_model.is_none() {
            Some(state.selected_turn)
        } else {
            None
        };
        let mut model_jump = initial_model;
        let mut jump_block: Option<String> = None;
        let mut anchor: Option<ReadingAnchor> = None;
        let mut previous = Vec::<String>::new();
        let mut updated = false;
        let mut check_disk = Instant::now();
        loop {
            if stopped.load(Ordering::Relaxed) {
                return Ok(ReaderExit::Quit);
            }
            if let Ok(result) = results_rx.try_recv() {
                loading = false;
                match result {
                    Ok(next) => {
                        anchor = reading_anchor(&doc, state.offset);
                        conversation = Arc::new(next);
                        state.model_hits = model_hits(&conversation, cli, report);
                        state.selected_turn = state
                            .selected_turn
                            .min(conversation.turns.len().saturating_sub(1));
                        if let Some(token) = search_cancel.0.take() {
                            token.store(true, Ordering::Relaxed);
                        }
                        state.hits.clear();
                        state.hit = 0;
                        state.hits_limited = false;
                        state.searching = false;
                        search_generation += 1;
                        if !state.query.is_empty() {
                            let token = Arc::new(AtomicBool::new(false));
                            search_cancel.0 = Some(Arc::clone(&token));
                            let _ = search_tx.send(SearchRequest {
                                data: Arc::clone(&conversation),
                                query: state.query.clone(),
                                cancelled: token,
                                generation: search_generation,
                            });
                            state.searching = true;
                        }
                        updated = false;
                        rebuild = true;
                    }
                    Err(error) if crate::cancel::is_cancelled(&error) => {}
                    Err(error) => {
                        state.notice = Some(format!("Refresh failed: {error:#}"));
                    }
                }
                dirty = true;
            }
            if let Ok(found) = found_rx.try_recv() {
                if found.generation == search_generation && !state.query.is_empty() {
                    state.hits = found.hits;
                    state.hits_limited = found.limited;
                    state.hit = 0;
                    state.searching = false;
                    jump_block = reveal_hit(&conversation, &mut state);
                    rebuild = true;
                    dirty = true;
                }
            }
            let (w, h) = terminal::size().unwrap_or((100, 24));
            let width = w.saturating_sub(1).max(1) as usize;
            let height = h.max(1) as usize;
            let content_width = if width >= 120 {
                (width - 31).min(110)
            } else {
                width.min(110)
            };
            if old_width != width || old_height != height {
                if old_width != width {
                    if jump_turn.is_none() && jump_block.is_none() && !state.details && !state.help
                    {
                        anchor = reading_anchor(&doc, state.offset);
                    }
                    rebuild = true;
                }
                old_width = width;
                old_height = height;
                dirty = true;
                previous.clear();
                execute!(io::stdout(), Clear(ClearType::All))?;
            }
            if check_disk.elapsed() >= Duration::from_secs(2) {
                let changed = conversation::changed(&conversation);
                if changed != updated {
                    updated = changed;
                    dirty = true;
                }
                check_disk = Instant::now();
            }
            if rebuild || document_width != content_width {
                doc = document(
                    &conversation,
                    &state,
                    content_width.saturating_sub(2),
                    tz,
                    cli.ascii,
                    &stopped,
                )?;
                document_width = content_width;
                if let Some(saved) = anchor.take() {
                    state.offset = restore_anchor(&doc, &saved);
                }
                rebuild = false;
                dirty = true;
            }
            if let Some(turn) = jump_turn.take() {
                state.offset = locate_turn(&doc, turn);
            }
            if let Some(hit) = model_jump.take() {
                let page = chrome(
                    &conversation,
                    &state,
                    &Paint {
                        cli,
                        report,
                        style: &style,
                        width,
                        height,
                        updated,
                        loading,
                    },
                )
                .page;
                state.offset = model_offset(&doc, &hit, page);
            }
            if let Some(id) = jump_block.take() {
                if let Some(line) = locate_block(&doc, &id) {
                    state.offset = if !state.query.is_empty() {
                        hit_line(&doc, &state, &id).unwrap_or(line)
                    } else {
                        line
                    };
                }
            }
            let page = chrome(
                &conversation,
                &state,
                &Paint {
                    cli,
                    report,
                    style: &style,
                    width,
                    height,
                    updated,
                    loading,
                },
            )
            .page;
            let content_len = if state.details {
                details(&conversation, report, width).len()
            } else if state.help {
                help(width).len()
            } else {
                doc.lines.len()
            };
            state.offset = state.offset.min(content_len.saturating_sub(page));
            if !state.details && !state.help {
                let focused_turn = state.focused.as_ref().and_then(|id| {
                    doc.lines
                        .iter()
                        .skip(state.offset)
                        .take(page)
                        .find(|line| line.block.as_ref() == Some(id))
                        .map(|line| line.turn)
                });
                state.selected_turn = focused_turn
                    .or_else(|| doc.lines.get(state.offset).map(|line| line.turn))
                    .unwrap_or(state.selected_turn);
                keep_turn_visible(&mut state, page);
            }
            if dirty {
                let mut lines = frame(
                    &conversation,
                    &state,
                    &doc,
                    &Paint {
                        cli,
                        report,
                        style: &style,
                        width,
                        height,
                        updated,
                        loading,
                    },
                );
                if let Some(notice) = &state.notice {
                    if lines.len() > 2 {
                        lines[2] = style.paint("warn", short(notice, width));
                    }
                }
                let mut out = io::stdout().lock();
                for (y, line) in lines.iter().enumerate() {
                    if previous.get(y) != Some(line) {
                        queue!(
                            out,
                            MoveTo(0, y as u16),
                            Clear(ClearType::CurrentLine),
                            Print(line)
                        )?;
                    }
                }
                for y in lines.len()..previous.len() {
                    queue!(out, MoveTo(0, y as u16), Clear(ClearType::CurrentLine))?;
                }
                out.flush()?;
                previous = lines;
                dirty = false;
            }
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            match event::read()? {
                Event::Resize(_, _) => {
                    dirty = true;
                }
                Event::Key(key_event) if key_event.kind != KeyEventKind::Release => {
                    if key_event.code == KeyCode::Char('c')
                        && key_event.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        stopped.store(true, Ordering::Relaxed);
                        return Ok(ReaderExit::Quit);
                    }
                    if let Some(input) = &mut state.search_input {
                        match key_event.code {
                            KeyCode::Esc => {
                                state.search_input = None;
                            }
                            KeyCode::Enter => {
                                state.query = state.search_input.take().unwrap_or_default();
                                if let Some(token) = search_cancel.0.take() {
                                    token.store(true, Ordering::Relaxed);
                                }
                                state.hits.clear();
                                state.hit = 0;
                                state.hits_limited = false;
                                state.searching = false;
                                search_generation += 1;
                                if !state.query.is_empty() {
                                    let token = Arc::new(AtomicBool::new(false));
                                    search_cancel.0 = Some(Arc::clone(&token));
                                    let _ = search_tx.send(SearchRequest {
                                        data: Arc::clone(&conversation),
                                        query: state.query.clone(),
                                        cancelled: token,
                                        generation: search_generation,
                                    });
                                    state.searching = true;
                                }
                                jump_block = reveal_hit(&conversation, &mut state);
                                rebuild = true;
                            }
                            KeyCode::Backspace => {
                                input.pop();
                            }
                            KeyCode::Char(c)
                                if !key_event.modifiers.contains(KeyModifiers::CONTROL)
                                    && input.len() < 1024 =>
                            {
                                input.push(c)
                            }
                            _ => {}
                        }
                        dirty = true;
                        continue;
                    }
                    match key_event.code {
                        KeyCode::Char('q') => return Ok(ReaderExit::Quit),
                        KeyCode::Esc => {
                            if state.details || state.help {
                                state.details = false;
                                state.help = false;
                                state.offset = state
                                    .saved_anchor
                                    .take()
                                    .map(|anchor| restore_anchor(&doc, &anchor))
                                    .unwrap_or(state.saved_offset);
                            } else if !state.query.is_empty() {
                                state.query.clear();
                                state.hits.clear();
                                state.hits_limited = false;
                                state.searching = false;
                                search_generation += 1;
                                if let Some(token) = search_cancel.0.take() {
                                    token.store(true, Ordering::Relaxed);
                                }
                            } else {
                                return Ok(ReaderExit::Back);
                            }
                        }
                        KeyCode::Char('?') => {
                            if !state.help && !state.details {
                                state.saved_offset = state.offset;
                                state.saved_anchor = reading_anchor(&doc, state.offset);
                            }
                            state.help = !state.help;
                            state.details = false;
                            state.offset = if state.help {
                                0
                            } else {
                                state
                                    .saved_anchor
                                    .take()
                                    .map(|anchor| restore_anchor(&doc, &anchor))
                                    .unwrap_or(state.saved_offset)
                            };
                        }
                        KeyCode::Char('i') => {
                            if !state.help && !state.details {
                                state.saved_offset = state.offset;
                                state.saved_anchor = reading_anchor(&doc, state.offset);
                            }
                            state.details = !state.details;
                            state.help = false;
                            state.offset = if state.details {
                                0
                            } else {
                                state
                                    .saved_anchor
                                    .take()
                                    .map(|anchor| restore_anchor(&doc, &anchor))
                                    .unwrap_or(state.saved_offset)
                            };
                        }
                        KeyCode::Char('/') => {
                            if state.help || state.details {
                                state.help = false;
                                state.details = false;
                                state.offset = state
                                    .saved_anchor
                                    .take()
                                    .map(|anchor| restore_anchor(&doc, &anchor))
                                    .unwrap_or(state.saved_offset);
                            }
                            state.search_input = Some(state.query.clone());
                        }
                        KeyCode::Char('n') | KeyCode::Char('N')
                            if !state.help && !state.details =>
                        {
                            if !state.hits.is_empty() {
                                state.hit = if key_event.code == KeyCode::Char('N') {
                                    (state.hit + state.hits.len() - 1) % state.hits.len()
                                } else {
                                    (state.hit + 1) % state.hits.len()
                                };
                                jump_block = reveal_hit(&conversation, &mut state);
                                rebuild = true;
                            }
                        }
                        KeyCode::Char('t') if !state.help && !state.details => {
                            anchor = reading_anchor(&doc, state.offset);
                            state.tools = !state.tools;
                            rebuild = true;
                        }
                        KeyCode::Tab | KeyCode::BackTab if !state.help && !state.details => {
                            focus_next(&mut state, &doc, key_event.code == KeyCode::BackTab, page);
                        }
                        KeyCode::Enter if !state.help && !state.details => {
                            if let Some(id) = state
                                .focused
                                .clone()
                                .filter(|id| block_visible(&doc, id, state.offset, page))
                                .or_else(|| top_block(&doc, state.offset))
                            {
                                if id == "__earlier_context" {
                                    state.inherited = !state.inherited;
                                } else if id.starts_with("tools:") {
                                    state.tools = true;
                                    jump_block = Some(id.trim_start_matches("tools:").into());
                                } else if state.expanded.contains(&id) && doc.folded.contains(&id) {
                                    *state.limits.entry(id.clone()).or_insert(2000) += 2000;
                                } else if !state.expanded.remove(&id) {
                                    state.expanded.insert(id.clone());
                                    state.limits.insert(id.clone(), 2000);
                                } else {
                                    state.limits.remove(&id);
                                    state.windows.remove(&id);
                                }
                                if jump_block.is_none() {
                                    anchor = reading_anchor(&doc, state.offset);
                                }
                                rebuild = true;
                            }
                        }
                        KeyCode::Backspace if !state.help && !state.details => {
                            if let Some(id) = state
                                .focused
                                .clone()
                                .filter(|id| block_visible(&doc, id, state.offset, page))
                                .or_else(|| top_block(&doc, state.offset))
                            {
                                state.expanded.remove(&id);
                                state.limits.remove(&id);
                                state.windows.remove(&id);
                                anchor = reading_anchor(&doc, state.offset);
                                rebuild = true;
                            }
                        }
                        KeyCode::Char('[') | KeyCode::Char(']')
                            if !state.help && !state.details =>
                        {
                            if !conversation.turns.is_empty() {
                                let current = state.selected_turn;
                                let next = if key_event.code == KeyCode::Char('[') {
                                    current.saturating_sub(1)
                                } else {
                                    (current + 1).min(conversation.turns.len() - 1)
                                };
                                if conversation.turns[next].inherited && !state.inherited {
                                    state.inherited = true;
                                    rebuild = true;
                                }
                                state.selected_turn = next;
                                state.focused = conversation.turns[next].blocks.first().map(key);
                                jump_turn = Some(next);
                            }
                        }
                        KeyCode::Char('m') | KeyCode::Char('M')
                            if !state.help && !state.details =>
                        {
                            if let Some(turn) =
                                jump_model(&mut state, key_event.code == KeyCode::Char('M'))
                            {
                                model_jump = state
                                    .model_hits
                                    .iter()
                                    .find(|hit| hit.turn == turn)
                                    .cloned();
                                if let Some(hit) = &model_jump {
                                    let tools = state.tools;
                                    reveal_model(&conversation, &mut state, hit);
                                    rebuild |= tools != state.tools;
                                }
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            scroll(&mut state, content_len, page, Scroll::Down)
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            scroll(&mut state, content_len, page, Scroll::Up)
                        }
                        KeyCode::PageDown | KeyCode::Char(' ') => {
                            scroll(&mut state, content_len, page, Scroll::PageDown)
                        }
                        KeyCode::PageUp => scroll(&mut state, content_len, page, Scroll::PageUp),
                        KeyCode::Home | KeyCode::Char('g') => {
                            scroll(&mut state, content_len, page, Scroll::Home)
                        }
                        KeyCode::End | KeyCode::Char('G') => {
                            scroll(&mut state, content_len, page, Scroll::End)
                        }
                        KeyCode::Char('r') if !loading && !state.help && !state.details => {
                            reload_stop.store(false, Ordering::Relaxed);
                            if requests_tx.send(()).is_ok() {
                                loading = true;
                                state.notice = None;
                            }
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        conversation::{ConversationFile, Turn},
        model::Scan,
        report::{self, Counts, Row},
    };
    use clap::Parser;
    use rust_decimal::Decimal;
    fn block(id: &str, kind: BlockKind, text: &str) -> Block {
        Block {
            id: id.into(),
            kind,
            timestamp: Some("2026-11-04T14:00:00Z".parse().unwrap()),
            text: text.into(),
            model: Some("model-test".into()),
            mode: Some("fast".into()),
            call_id: None,
            tool_name: (kind == BlockKind::Tool).then(|| "exec_command".into()),
            output: None,
            status: None,
            duration_ms: None,
            source: "local".into(),
            path: "/fixture/session.jsonl".into(),
            line: 1,
        }
    }
    fn fixture() -> (Cli, Conversation, Report) {
        let cli = Cli::parse_from([
            "codex-usage",
            "sessions",
            "--all",
            "--no-cost",
            "--color",
            "never",
        ]);
        let report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        let conversation = Conversation {
            session_id: "session-test".into(),
            project: Some("/work/project".into()),
            sources: vec!["local".into()],
            files: Vec::<ConversationFile>::new(),
            turns: vec![
                Turn {
                    id: "turn-1".into(),
                    timestamp: Some("2026-11-04T14:00:00Z".parse().unwrap()),
                    title: "Read my project".into(),
                    blocks: vec![
                        block("user-1", BlockKind::User, "Read my project"),
                        block(
                            "assistant-1",
                            BlockKind::Assistant,
                            "I'll inspect the source.",
                        ),
                        block("tool-1", BlockKind::Tool, "rg --files"),
                    ],
                    inherited: false,
                },
                Turn {
                    id: "turn-2".into(),
                    timestamp: Some("2026-11-04T14:05:00Z".parse().unwrap()),
                    title: "Explain the result".into(),
                    blocks: vec![
                        block("user-2", BlockKind::User, "Explain the result"),
                        block("assistant-2", BlockKind::Assistant, "Here is the result."),
                    ],
                    inherited: false,
                },
            ],
            warnings: vec![],
            parent_session: None,
            pending: 0,
        };
        (cli, conversation, report)
    }
    #[test]
    fn layout_wraps_and_adapts_without_losing_footer_or_tool_expansion() {
        let (cli, conversation, report) = fixture();
        let style = Style::new(&cli, false);
        let stopped = AtomicBool::new(false);
        for width in [40, 59, 80, 120, 200] {
            for height in [12, 25, 45] {
                let state = State::default();
                let pane = if width >= 120 {
                    (width - 31).min(110)
                } else {
                    width.min(110)
                };
                let doc = document(
                    &conversation,
                    &state,
                    pane - 2,
                    chrono_tz::UTC,
                    false,
                    &stopped,
                )
                .unwrap();
                let lines = frame(
                    &conversation,
                    &state,
                    &doc,
                    &Paint {
                        cli: &cli,
                        report: &report,
                        style: &style,
                        width,
                        height,
                        updated: false,
                        loading: false,
                    },
                );
                assert_eq!(lines.len(), height);
                assert!(
                    lines.iter().all(|line| line.width() <= width),
                    "{width}x{height}: {lines:?}"
                );
                assert!(
                    lines.last().unwrap().contains("Back")
                        || lines.iter().rev().take(3).any(|s| s.contains("Back")),
                    "{lines:?}"
                );
                if width >= 120 {
                    assert!(lines.iter().any(|s| s.starts_with("Turns")));
                }
                assert!(doc
                    .lines
                    .iter()
                    .any(|line| line.text.contains("1 tool calls")));
                assert!(!doc
                    .lines
                    .iter()
                    .any(|line| line.text.contains("rg --files")));
            }
        }
        let state = State {
            tools: true,
            expanded: BTreeSet::from(["tool-1".into()]),
            ..Default::default()
        };
        let doc = document(&conversation, &state, 70, chrono_tz::UTC, true, &stopped).unwrap();
        assert!(doc.lines.iter().any(|line| line.text == "rg --files"));
    }
    #[test]
    fn deep_tool_search_reveals_exact_matches_without_rendering_every_preceding_line() {
        let (_, mut conversation, _) = fixture();
        conversation.turns[0].blocks[2].output = Some(
            (0..400)
                .map(|i| {
                    if i == 300 || i == 350 {
                        format!("line{i} NEEDLE éClair\n")
                    } else {
                        format!("line{i} ordinary output\n")
                    }
                })
                .collect(),
        );
        let stopped = AtomicBool::new(false);
        let (hits, limited) = search(&conversation, "needle", &stopped).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(!limited);
        let mut state = State {
            query: "needle".into(),
            hits,
            ..Default::default()
        };
        for index in 0..2 {
            state.hit = index;
            let id = reveal_hit(&conversation, &mut state).unwrap();
            let doc = document(&conversation, &state, 78, chrono_tz::UTC, false, &stopped).unwrap();
            let line = hit_line(&doc, &state, &id).unwrap();
            assert!(doc.lines[line]
                .text
                .contains(if index == 0 { "line300" } else { "line350" }));
            assert!(state.tools);
            assert!(doc.lines.len() < 160);
        }
        assert_eq!(
            search(&conversation, "ÉCLAIR", &stopped).unwrap().0.len(),
            2
        );
        assert!(crate::cancel::is_cancelled(
            &search(&conversation, "needle", &AtomicBool::new(true)).unwrap_err()
        ));
    }
    #[test]
    fn giant_single_line_is_bounded_in_reader_and_complete_in_plain_export() {
        let (_, mut conversation, _) = fixture();
        conversation.turns[0].blocks[0].text = format!("{} RETAINED_END", "x".repeat(250_000));
        let stopped = AtomicBool::new(false);
        let mut state = State::default();
        let doc = document(&conversation, &state, 60, chrono_tz::UTC, true, &stopped).unwrap();
        assert!(doc.folded.contains("user-1"));
        assert!(doc.lines.len() < 60);
        state.expanded.insert("user-1".into());
        state.limits.insert("user-1".into(), 2000);
        let doc = document(&conversation, &state, 60, chrono_tz::UTC, true, &stopped).unwrap();
        assert!(doc.folded.contains("user-1"));
        assert!(doc.lines.len() < 2040);
        let plain = plain(&conversation, chrono_tz::UTC);
        assert!(plain.contains("RETAINED_END"));
        assert!(plain.len() > 250_000);
    }
    #[test]
    fn session_details_use_authoritative_totals_or_known_selected_subtotal() {
        let (_, conversation, mut report) = fixture();
        report.kind = "Session".into();
        report.totals.total = 123;
        report.totals.cost_usd = Some(Decimal::new(123, 2));
        // A direct session report may be grouped by model and have no session column.
        let text = details(&conversation, &report, 120).join("\n");
        assert!(text.contains("123 tokens / $1.2300"));
        assert!(text.contains("Usage snapshot:"));
        report.kind = "Sessions".into();
        report.rows = vec![
            Row {
                dimensions: BTreeMap::from([("session".into(), conversation.session_id.clone())]),
                counts: Counts {
                    total: 100,
                    cost_usd: Some(Decimal::new(25, 2)),
                    ..Default::default()
                },
            },
            Row {
                dimensions: BTreeMap::from([("session".into(), conversation.session_id.clone())]),
                counts: Counts {
                    total: 20,
                    cost_usd: None,
                    unpriced_tokens: 20,
                    ..Default::default()
                },
            },
            Row {
                dimensions: BTreeMap::from([("session".into(), "other".into())]),
                counts: Counts {
                    total: 999,
                    cost_usd: Some(Decimal::from(999)),
                    ..Default::default()
                },
            },
        ];
        let text = details(&conversation, &report, 120).join("\n");
        assert!(text.contains("120 tokens / $0.2500*"));
        assert!(!text.contains("999 tokens"));
    }
    #[test]
    fn inherited_context_and_initial_anchor_respect_reader_scope() {
        let (_, mut conversation, mut report) = fixture();
        conversation.turns[0].inherited = true;
        let stopped = AtomicBool::new(false);
        let mut state = State::default();
        let doc = document(&conversation, &state, 70, chrono_tz::UTC, false, &stopped).unwrap();
        assert!(doc
            .lines
            .iter()
            .any(|line| line.text.contains("Earlier context")));
        assert!(!doc
            .lines
            .iter()
            .any(|line| line.text.contains("Read my project")));
        state.inherited = true;
        let doc = document(&conversation, &state, 70, chrono_tz::UTC, false, &stopped).unwrap();
        assert!(doc
            .lines
            .iter()
            .any(|line| line.text.contains("Read my project")));
        conversation.turns[0].inherited = false;
        assert_eq!(initial_turn(&conversation, &report, false), 1);
        assert_eq!(initial_turn(&conversation, &report, true), 1);
        report.period.start = Some("2026-11-04T13:00:00Z".parse().unwrap());
        assert_eq!(initial_turn(&conversation, &report, true), 0);
    }
    #[test]
    fn paging_uses_rendered_page_and_stops_at_the_last_full_viewport() {
        let (cli, conversation, report) = fixture();
        let style = Style::new(&cli, false);
        for width in [40, 59, 60, 80, 120] {
            for height in [12, 25, 45] {
                for updated in [false, true] {
                    let mut state = State {
                        offset: 2,
                        focused: Some("stale".into()),
                        ..Default::default()
                    };
                    let chrome = chrome(
                        &conversation,
                        &state,
                        &Paint {
                            cli: &cli,
                            report: &report,
                            style: &style,
                            width,
                            height,
                            updated,
                            loading: false,
                        },
                    );
                    assert_eq!(
                        chrome.header.len() + chrome.page + chrome.footer.len(),
                        height
                    );
                    let page = chrome.page;
                    scroll(&mut state, 500, page, Scroll::PageDown);
                    assert_eq!(state.offset, 2 + page);
                    assert!(state.focused.is_none());
                    scroll(&mut state, 500, page, Scroll::PageUp);
                    assert_eq!(state.offset, 2);
                    scroll(&mut state, 500, page, Scroll::End);
                    let end = state.offset;
                    assert_eq!(end, 500 - page);
                    for _ in 0..600 {
                        scroll(&mut state, 500, page, Scroll::Down);
                    }
                    assert_eq!(state.offset, end);
                    scroll(&mut state, 500, page, Scroll::PageUp);
                    scroll(&mut state, 500, page, Scroll::PageDown);
                    assert_eq!(state.offset, end);
                }
            }
        }
    }
    #[test]
    fn tab_from_a_scrolled_block_ignores_stale_offscreen_focus() {
        let (_, mut conversation, _) = fixture();
        conversation.turns[0].blocks[0].text =
            (0..250).map(|i| format!("retained line {i}\n")).collect();
        let mut state = State {
            expanded: BTreeSet::from(["user-1".into()]),
            offset: 100,
            focused: Some("assistant-2".into()),
            ..Default::default()
        };
        let doc = document(
            &conversation,
            &state,
            70,
            chrono_tz::UTC,
            true,
            &AtomicBool::new(false),
        )
        .unwrap();
        focus_next(&mut state, &doc, false, 20);
        assert_eq!(state.focused.as_deref(), Some("user-1"));
        assert_eq!(state.offset, 100);
        scroll(&mut state, doc.lines.len(), 20, Scroll::PageDown);
        assert_eq!(state.offset, 120);
        assert!(state.focused.is_none());
        focus_next(&mut state, &doc, true, 20);
        assert_eq!(state.focused.as_deref(), Some("user-1"));
        assert_eq!(state.offset, 120);
    }
    #[test]
    fn source_anchor_preserves_reading_position_through_reflow_and_reload() {
        let (_, mut conversation, _) = fixture();
        conversation.turns[0].blocks[0].text = (0..150)
            .map(|i| {
                format!("line{i:03} 模型 and preserved content that can wrap on narrower screens\n")
            })
            .collect();
        let state = State {
            expanded: BTreeSet::from(["user-1".into()]),
            ..Default::default()
        };
        let stop = AtomicBool::new(false);
        let old = document(&conversation, &state, 78, chrono_tz::UTC, false, &stop).unwrap();
        let offset = old
            .lines
            .iter()
            .position(|line| line.text.starts_with("line077"))
            .unwrap();
        let anchor = reading_anchor(&old, offset).unwrap();
        assert_eq!(
            restore_anchor(&old, &anchor),
            offset,
            "height-only redraw keeps exact offset"
        );
        for width in [38, 88, 108] {
            let next =
                document(&conversation, &state, width, chrono_tz::UTC, false, &stop).unwrap();
            let index = restore_anchor(&next, &anchor);
            assert!(
                next.lines[index].text.starts_with("line077"),
                "{}",
                next.lines[index].text
            );
            assert_eq!(next.lines[index].source, anchor.source);
        }
        conversation.turns[0].blocks[0]
            .text
            .push_str("new trailing text\n");
        let next = document(&conversation, &state, 78, chrono_tz::UTC, false, &stop).unwrap();
        assert_eq!(restore_anchor(&next, &anchor), offset);
    }
    #[test]
    fn selected_model_anchors_real_assistant_or_tool_context_not_user_metadata() {
        let (mut cli, mut conversation, mut report) = fixture();
        cli.models = vec!["gpt-5.6-sol".into()];
        cli.mode = Some(crate::cli::Mode::Fast);
        conversation.turns[0].blocks[0].model = Some("gpt-5.6-sol".into());
        conversation.turns[0].blocks[0].mode = Some("fast".into());
        let assistant = conversation.turns[1]
            .blocks
            .iter_mut()
            .find(|b| b.kind == BlockKind::Assistant)
            .unwrap();
        assistant.model = Some("gpt-5.6-sol".into());
        assistant.mode = Some("fast".into());
        assistant.timestamp = Some("2026-11-04T14:05:00Z".parse().unwrap());
        let hits = model_hits(&conversation, &cli, &report);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].turn, 1);
        let mut state = State {
            model_hits: hits,
            ..Default::default()
        };
        assert_eq!(jump_model(&mut state, false), Some(1));
        assert_eq!(state.focused.as_deref(), Some("assistant-2"));
        let (hits, _) = search(&conversation, "gpt-5.6-sol", &AtomicBool::new(false)).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "user labels must not invent model invocations"
        );
        assert_eq!(hits[0].part, Part::Heading);
        state.query = "gpt-5.6-sol".into();
        state.hits = hits;
        let id = reveal_hit(&conversation, &mut state).unwrap();
        let doc = document(
            &conversation,
            &state,
            78,
            chrono_tz::UTC,
            false,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(doc.lines[hit_line(&doc, &state, &id).unwrap()]
            .text
            .contains("gpt-5.6-sol"));
        report.period.end = Some("2026-11-04T14:01:00Z".parse().unwrap());
        assert!(model_hits(&conversation, &cli, &report).is_empty());
        report.period.end = None;
        conversation.turns[1].inherited = true;
        assert!(model_hits(&conversation, &cli, &report).is_empty());
    }
    #[test]
    fn outline_moves_only_when_selection_leaves_its_visible_rows() {
        let mut state = State::default();
        for selected in 0..9 {
            state.selected_turn = selected;
            keep_turn_visible(&mut state, 10);
            assert_eq!(state.outline_offset, 0);
        }
        state.selected_turn = 9;
        keep_turn_visible(&mut state, 10);
        assert_eq!(state.outline_offset, 1);
        state.selected_turn = 2;
        keep_turn_visible(&mut state, 10);
        assert_eq!(state.outline_offset, 1);
        state.selected_turn = 0;
        keep_turn_visible(&mut state, 10);
        assert_eq!(state.outline_offset, 0);
    }
    #[test]
    fn model_navigation_reveals_invocations_after_long_prompts_and_hidden_tools() {
        for kind in [BlockKind::Assistant, BlockKind::Tool] {
            let (mut cli, mut conversation, report) = fixture();
            cli.models = vec!["gpt-5.6-sol".into()];
            cli.mode = Some(crate::cli::Mode::Fast);
            conversation.turns[0].blocks[0].text =
                (0..50).map(|i| format!("long prompt line {i}\n")).collect();
            let target = conversation.turns[0]
                .blocks
                .iter_mut()
                .find(|block| block.kind == kind)
                .unwrap();
            target.model = Some("gpt-5.6-sol".into());
            target.mode = Some("fast".into());
            let hit = model_hits(&conversation, &cli, &report).remove(0);
            let mut state = State::default();
            reveal_model(&conversation, &mut state, &hit);
            assert_eq!(state.tools, kind == BlockKind::Tool);
            let doc = document(
                &conversation,
                &state,
                78,
                chrono_tz::UTC,
                false,
                &AtomicBool::new(false),
            )
            .unwrap();
            let offset = model_offset(&doc, &hit, 10);
            let target = locate_block(&doc, &hit.block).unwrap();
            assert!(
                offset <= target && target < offset + 10,
                "matching invocation must be visible"
            );
            assert!(doc.lines[target].text.contains("gpt-5.6-sol"));
            assert!(
                offset > locate_turn(&doc, hit.turn),
                "long prompts must not hide target below viewport"
            );
        }
    }
    #[test]
    fn model_focus_preserves_partial_warning_and_only_advertises_real_matches() {
        let (cli, mut conversation, report) = fixture();
        conversation
            .warnings
            .push("Incomplete retained message".into());
        let mut state = State {
            model_focus: "gpt-5.6-sol / Fast".into(),
            ..Default::default()
        };
        let style = Style::new(&cli, false);
        let paint = Paint {
            cli: &cli,
            report: &report,
            style: &style,
            width: 80,
            height: 25,
            updated: false,
            loading: false,
        };
        let c = chrome(&conversation, &state, &paint);
        assert!(c.header.join("\n").contains("partial data"));
        assert!(!c.footer.join("\n").contains("m/M Model"));
        state.model_hits.push(ModelHit {
            turn: 0,
            block: "assistant-1".into(),
        });
        assert!(chrome(&conversation, &state, &paint)
            .footer
            .join("\n")
            .contains("m/M Model"));
    }
}
