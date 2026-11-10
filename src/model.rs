use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    str::FromStr,
};

/// Statically selected agent implementation. Adapters share consumers but never
/// share native identities.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    #[default]
    Codex,
    ClaudeCode,
}
impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
        }
    }
}
impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaCapability {
    RecordedSnapshots,
    #[default]
    Unsupported,
}
impl QuotaCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecordedSnapshots => "recorded_snapshots",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum UsageMode {
    Regular,
    Fast,
    Flex,
    Ultrafast,
    Batch,
    #[default]
    Unknown,
}
impl UsageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Fast => "fast",
            Self::Flex => "flex",
            Self::Ultrafast => "ultrafast",
            Self::Batch => "batch",
            Self::Unknown => "unknown",
        }
    }
}
impl fmt::Display for UsageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub agent: AgentKind,
    pub kind: String,
    pub root: PathBuf,
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Explicit API-equivalent pricing scenario used only when the recorded
    /// provider is unknown. Never rewrites the recorded provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_provider: Option<String>,
}
fn enabled() -> bool {
    true
}

/// Source configuration identity. A root with a different agent is a different
/// source, so changing either field invalidates cached file state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    pub name: String,
    pub agent: AgentKind,
    pub normalized_root: PathBuf,
}
impl SourceKey {
    pub fn new(source: &Source) -> Self {
        Self {
            name: source.name.clone(),
            agent: source.agent,
            normalized_root: source.root.clone(),
        }
    }
}

/// Strong session identity: agent plus native session plus optional execution
/// (Claude agentId). Copied transcripts share a key; equal native IDs under
/// different agents do not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent: AgentKind,
    pub native_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}
impl SessionKey {
    pub fn main(agent: AgentKind, native_id: impl Into<String>) -> Self {
        Self {
            agent,
            native_id: native_id.into(),
            execution_id: None,
        }
    }
    pub fn execution(
        agent: AgentKind,
        native_id: impl Into<String>,
        execution_id: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            native_id: native_id.into(),
            execution_id: Some(execution_id.into()),
        }
    }
    pub fn is_main(&self) -> bool {
        self.execution_id.is_none()
    }
    /// Canonical `<agent>:<percent-encoded-native>[:<percent-encoded-execution>]`.
    pub fn selector(&self) -> String {
        let mut out = format!("{}:{}", self.agent, encode_component(&self.native_id));
        if let Some(execution) = &self.execution_id {
            out.push(':');
            out.push_str(&encode_component(execution));
        }
        out
    }
}
impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.selector())
    }
}
impl FromStr for SessionKey {
    type Err = String;
    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        parse_selector(text)
    }
}

/// Adapter-owned opaque billing-event identity. Never contains a source name or
/// root, so mirrored copies collapse while cross-agent collisions cannot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventKey {
    pub agent: AgentKind,
    pub native_id: String,
}
impl EventKey {
    pub fn new(agent: AgentKind, native_id: impl Into<String>) -> Self {
        Self {
            agent,
            native_id: native_id.into(),
        }
    }
}
impl fmt::Display for EventKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.agent, self.native_id)
    }
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-.~";

pub fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
fn decode_component(value: &str) -> std::result::Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let Some(hex) = bytes.get(index + 1..index + 3) else {
                    return Err("truncated percent escape".into());
                };
                let text = std::str::from_utf8(hex).map_err(|_| "invalid percent escape")?;
                let byte = u8::from_str_radix(text, 16).map_err(|_| "invalid percent escape")?;
                out.push(byte);
                index += 3;
            }
            byte if UNRESERVED.contains(&byte) => {
                out.push(byte);
                index += 1;
            }
            _ => return Err("unexpected character in session component".into()),
        }
    }
    String::from_utf8(out).map_err(|_| "session component is not UTF-8".into())
}

/// Parse an exact `<agent>:<native>[:<execution>]` selector.
pub fn parse_selector(text: &str) -> std::result::Result<SessionKey, String> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err("session selector must be <agent>:<session>[:<execution>]".into());
    }
    let agent = match parts[0] {
        "codex" => AgentKind::Codex,
        "claude-code" => AgentKind::ClaudeCode,
        other => return Err(format!("unknown session agent {other:?}")),
    };
    let native_id = decode_component(parts[1])?;
    if native_id.is_empty() {
        return Err("session selector has an empty session ID".into());
    }
    let execution_id = match parts.get(2) {
        Some(value) => {
            let value = decode_component(value)?;
            if value.is_empty() {
                return Err("session selector has an empty execution ID".into());
            }
            Some(value)
        }
        None => None,
    };
    Ok(SessionKey {
        agent,
        native_id,
        execution_id,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cached: Option<u64>,
    pub cache_write: Option<u64>,
    pub output: u64,
    pub reasoning: Option<u64>,
}
impl Usage {
    pub fn total(&self) -> u64 {
        self.input.saturating_add(self.output)
    }
    pub fn validate(&self) -> Result<()> {
        if self.input.checked_add(self.output).is_none() {
            bail!("token total overflow");
        }
        if self.cached.is_some_and(|v| v > self.input)
            || self.cache_write.is_some_and(|v| v > self.input)
            || self.reasoning.is_some_and(|v| v > self.output)
        {
            bail!("token subset exceeds its total");
        }
        Ok(())
    }
}

/// Cache-write tokens grouped by verified TTL. `unknown_ttl` holds writes whose
/// duration was not recorded and therefore has no default rate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheWriteBreakdown {
    #[serde(default)]
    pub by_ttl_seconds: BTreeMap<u64, u64>,
    #[serde(default)]
    pub unknown_ttl: u64,
}
impl CacheWriteBreakdown {
    pub fn from_ttl(seconds: u64, tokens: u64) -> Self {
        Self {
            by_ttl_seconds: [(seconds, tokens)].into(),
            unknown_ttl: 0,
        }
    }
    pub fn unknown(tokens: u64) -> Self {
        Self {
            by_ttl_seconds: BTreeMap::new(),
            unknown_ttl: tokens,
        }
    }
    pub fn total(&self) -> Option<u64> {
        self.by_ttl_seconds
            .values()
            .try_fold(self.unknown_ttl, |total, value| total.checked_add(*value))
    }
    /// A breakdown is usable only if no zero-duration key exists and, when an
    /// aggregate is known, the buckets account for it exactly.
    pub fn validate(&self, aggregate: Option<u64>) -> Result<()> {
        if self.by_ttl_seconds.keys().any(|seconds| *seconds == 0) {
            bail!("cache-write breakdown has a zero TTL");
        }
        let total = self
            .total()
            .ok_or_else(|| anyhow::anyhow!("cache-write breakdown overflow"))?;
        if aggregate.is_some_and(|value| value != total) {
            bail!("cache-write breakdown does not match the recorded total");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageEvent {
    pub id: EventKey,
    pub session: SessionKey,
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub provider: String,
    pub tier: Option<String>,
    #[serde(default)]
    pub mode: UsageMode,
    pub project: Option<String>,
    pub usage: Usage,
    /// `None` means the adapter did not record a TTL breakdown (legacy Codex
    /// retention semantics); `Some` is authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_writes: Option<CacheWriteBreakdown>,
    pub source: String,
    #[serde(default)]
    pub origins: BTreeSet<String>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
}
impl UsageEvent {
    pub fn mode(&self) -> &str {
        self.mode.as_str()
    }
    pub fn usage_mode(&self) -> UsageMode {
        self.mode
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub files: usize,
    pub malformed: usize,
    pub pending: usize,
    pub duplicates: usize,
    pub conflicts: usize,
    pub uncertain: usize,
    /// Identified event groups (or unidentified records) excluded from totals
    /// because their accounting was unusable.
    #[serde(default)]
    pub excluded_usage: usize,
    pub messages: BTreeSet<String>,
}
impl Coverage {
    pub fn incomplete(&self) -> bool {
        self.malformed > 0
            || self.pending > 0
            || self.conflicts > 0
            || self.uncertain > 0
            || self.excluded_usage > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    pub agent: AgentKind,
    pub root: PathBuf,
    pub readable: bool,
    pub last_success: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub capability: QuotaCapability,
    pub coverage: Coverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_provider: Option<String>,
}
impl Default for SourceStatus {
    fn default() -> Self {
        Self {
            name: String::new(),
            agent: AgentKind::Codex,
            root: PathBuf::new(),
            readable: false,
            last_success: None,
            error: None,
            capability: QuotaCapability::Unsupported,
            coverage: Coverage::default(),
            pricing_provider: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub source: String,
    pub path: PathBuf,
    pub layout: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub key: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub origins: BTreeSet<String>,
    #[serde(default)]
    pub files: Vec<SessionFile>,
}
impl SessionInfo {
    pub fn new(key: SessionKey) -> Self {
        Self {
            key,
            parent: None,
            project: None,
            origins: BTreeSet::new(),
            files: Vec::new(),
        }
    }
    pub fn merge(&mut self, other: &SessionInfo) {
        if self.parent.is_none() {
            self.parent.clone_from(&other.parent);
        }
        if self.project.is_none() {
            self.project.clone_from(&other.project);
        }
        self.origins.extend(other.origins.iter().cloned());
        for file in &other.files {
            if !self
                .files
                .iter()
                .any(|existing| existing.source == file.source && existing.path == file.path)
            {
                self.files.push(file.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scan {
    pub events: Vec<UsageEvent>,
    #[serde(default)]
    pub quota_snapshots: Vec<crate::quota::QuotaSnapshot>,
    pub coverage: Coverage,
    pub sources: Vec<SourceStatus>,
    #[serde(default)]
    pub sessions: BTreeMap<SessionKey, SessionInfo>,
    /// Includes identified records excluded for conflicts or incomplete usage, so
    /// the monitor can distinguish newly observed work from later repairs.
    #[serde(default)]
    pub observed_event_ids: BTreeSet<EventKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_round_trip_and_keep_reserved_characters_encoded() {
        for key in [
            SessionKey::main(AgentKind::Codex, "session-123"),
            SessionKey::main(AgentKind::ClaudeCode, "abc.def_ghi-jkl~"),
            SessionKey::execution(AgentKind::ClaudeCode, "session-123", "agent-456"),
            SessionKey::execution(AgentKind::ClaudeCode, "sessi:on/with space", "exec%id"),
            SessionKey::main(AgentKind::ClaudeCode, "会话"),
        ] {
            let text = key.selector();
            assert!(!text.contains(' '));
            assert_eq!(SessionKey::from_str(&text).unwrap(), key);
        }
        assert_eq!(
            SessionKey::execution(AgentKind::ClaudeCode, "a/b", "c:d").selector(),
            "claude-code:a%2Fb:c%3Ad"
        );
    }

    #[test]
    fn malformed_selectors_are_rejected() {
        for text in [
            "",
            "codex",
            "codex:",
            "codex:a:b:c",
            "other:a",
            "codex:a%2",
            "codex:a%zz",
            "codex:a%2Fb%",
            "claude-code:a b",
            "claude-code:a:",
        ] {
            assert!(SessionKey::from_str(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn raw_identical_ids_in_different_agents_remain_distinct() {
        let codex = EventKey::new(AgentKind::Codex, "message:same");
        let claude = EventKey::new(AgentKind::ClaudeCode, "message:same");
        assert_ne!(codex, claude);
        let sessions = BTreeSet::from([
            SessionKey::main(AgentKind::Codex, "same"),
            SessionKey::main(AgentKind::ClaudeCode, "same"),
        ]);
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn cache_breakdown_invariants_are_checked() {
        let mut breakdown = CacheWriteBreakdown::default();
        breakdown.by_ttl_seconds.insert(300, 150);
        breakdown.by_ttl_seconds.insert(3600, 50);
        breakdown.validate(Some(200)).unwrap();
        assert!(breakdown.validate(Some(201)).is_err());
        let zero = CacheWriteBreakdown::from_ttl(0, 1);
        assert!(zero.validate(None).is_err());
        let unknown = CacheWriteBreakdown::unknown(7);
        assert_eq!(unknown.total(), Some(7));
        assert!(unknown.validate(Some(6)).is_err());
    }
}
