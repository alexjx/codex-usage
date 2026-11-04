use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub kind: String,
    pub root: PathBuf,
    #[serde(default = "enabled")]
    pub enabled: bool,
}
fn enabled() -> bool {
    true
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub session: String,
    pub model: String,
    pub provider: String,
    pub tier: Option<String>,
    pub project: Option<String>,
    pub usage: Usage,
    pub source: String,
    #[serde(default)]
    pub origins: BTreeSet<String>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub cache_retention: Option<String>,
}
impl UsageEvent {
    pub fn mode(&self) -> &str {
        match self.tier.as_deref() {
            Some("default" | "standard") => "regular",
            Some("priority" | "fast") => "fast",
            Some("flex") => "flex",
            Some("ultrafast") => "ultrafast",
            Some("batch") => "batch",
            _ => "unknown",
        }
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
    pub messages: BTreeSet<String>,
}
impl Coverage {
    pub fn incomplete(&self) -> bool {
        self.malformed > 0 || self.pending > 0 || self.conflicts > 0 || self.uncertain > 0
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    pub root: PathBuf,
    pub readable: bool,
    pub last_success: Option<DateTime<Utc>>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scan {
    pub events: Vec<UsageEvent>,
    pub coverage: Coverage,
    pub sources: Vec<SourceStatus>,
}
