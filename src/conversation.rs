//! Shared selected-session conversation model and adapter dispatch.
//!
//! Conversation loading is independent from token accounting. Usage scans never
//! retain transcript bodies; text is read only when a conversation is requested.
use crate::{
    agents::{claude_conversation, codex_conversation},
    model::{AgentKind, SessionKey, Source},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub session: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionKey>,
    pub project: Option<String>,
    pub sources: Vec<String>,
    pub files: Vec<ConversationFile>,
    pub turns: Vec<Turn>,
    pub warnings: Vec<String>,
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

pub fn load(
    sources: &[Source],
    session: &SessionKey,
    stopped: &AtomicBool,
) -> Result<Conversation> {
    match session.agent {
        AgentKind::Codex => codex_conversation::load(sources, session, stopped),
        AgentKind::ClaudeCode => claude_conversation::load(sources, session, stopped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_newlines_and_removes_terminal_sequences() {
        let text = "a\n\u{1b}[31mred\u{1b}[0m\u{1b}]52;c;secret\u{7}b";
        assert_eq!(sanitize(text), "a\nredb");
    }
}
