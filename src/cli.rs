use chrono::NaiveDate;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Table,
    Json,
    Csv,
    Jsonl,
}
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Auto,
    Always,
    Never,
}
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Terminal,
    Dark,
    Light,
}
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pager {
    Auto,
    Always,
    Never,
}
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    Total,
    Input,
    Output,
    Cost,
    Model,
}
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Group {
    Model,
    Mode,
    Source,
    Day,
    Month,
    Session,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[value(alias = "standard", alias = "default")]
    Regular,
    #[value(alias = "priority")]
    Fast,
    Flex,
    Ultrafast,
    Batch,
    Unknown,
}
impl Mode {
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

#[derive(Debug, Clone, Parser, Serialize, Deserialize)]
#[command(
    version,
    about = "Local Codex usage reports and live monitoring",
    after_help = "Bare codex-usage repeats your last report; fresh reports default to this month. --last N selects N calendar days including today. Monitor always summarizes today. Costs are API-equivalent estimates, not subscription charges."
)]
pub struct Cli {
    #[arg(skip)]
    #[serde(skip)]
    pub explicit: Vec<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Configuration file (otherwise native application config directory).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Select named source; repeat to combine Windows and WSL.
    #[arg(long = "source", global = true, conflicts_with = "roots")]
    pub sources: Vec<String>,
    /// Read an ad hoc Codex home; also sets the path for `sources set`.
    #[arg(long = "root", global = true)]
    pub roots: Vec<PathBuf>,
    #[arg(long = "model", global = true)]
    pub models: Vec<String>,
    /// Filter recorded service mode; missing historical settings remain unknown.
    #[arg(long, global = true)]
    pub mode: Option<Mode>,
    #[arg(long = "session", global = true)]
    pub sessions: Vec<String>,
    /// Match project path or directory name.
    #[arg(long = "project", global = true)]
    pub projects: Vec<String>,
    #[arg(long, global = true, conflicts_with_all = ["all", "since", "until", "last"])]
    pub today: bool,
    #[arg(long, global = true, conflicts_with_all = ["today", "since", "until", "last"])]
    pub all: bool,
    #[arg(long, global = true)]
    pub since: Option<NaiveDate>,
    #[arg(long, global = true)]
    pub until: Option<NaiveDate>,
    /// Report the last N calendar days, including today.
    #[arg(long, global=true, value_parser=clap::value_parser!(u32).range(1..), conflicts_with_all=["today","all","since","until"])]
    pub last: Option<u32>,
    /// IANA timezone, e.g. Asia/Shanghai.
    #[arg(long, global = true)]
    pub timezone: Option<String>,
    #[arg(long, global = true, value_delimiter = ',')]
    pub group_by: Vec<Group>,
    #[arg(long, global = true)]
    pub sort: Option<Sort>,
    #[arg(long, global = true)]
    pub format: Option<Format>,
    #[arg(long, global = true, default_value = "auto")]
    pub color: Color,
    #[arg(long, global = true, default_value = "terminal")]
    pub theme: Theme,
    #[arg(long, global = true, default_value = "auto")]
    pub pager: Pager,
    #[arg(long, global = true)]
    pub ascii: bool,
    #[arg(long, global = true)]
    pub exact: bool,
    #[arg(long, global = true)]
    pub wide: bool,
    #[arg(long, global = true)]
    pub no_cost: bool,
    #[arg(long, global = true)]
    pub strict: bool,
    #[arg(long, global = true)]
    pub pricing_file: Option<PathBuf>,
    /// Reprice historical usage at a known rate date.
    #[arg(long, global = true)]
    pub pricing_date: Option<NaiveDate>,
}
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum Command {
    Models,
    Daily,
    Monthly,
    Sessions,
    Session {
        id: String,
    },
    /// Live summary of today's usage in the selected timezone.
    Monitor {
        #[arg(long, default_value = "2s", value_parser = parse_interval)]
        interval: std::time::Duration,
        /// Stop after N snapshots (useful for scripts).
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        frames: Option<u64>,
    },
    Sources {
        #[command(subcommand)]
        action: Option<SourceCommand>,
    },
    Pricing,
    Doctor,
}
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum SourceCommand {
    /// Set one source using --root PATH; other sources remain unchanged.
    Set {
        name: String,
        #[arg(long, value_parser = ["windows", "wsl", "linux"])]
        kind: Option<String>,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
}
fn parse_interval(value: &str) -> Result<std::time::Duration, String> {
    let (digits, multiplier) = if let Some(v) = value.strip_suffix("ms") {
        (v, 1)
    } else if let Some(v) = value.strip_suffix('s') {
        (v, 1000)
    } else {
        return Err("use a duration such as 2s or 1000ms".into());
    };
    let millis = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| (1000..=3_600_000).contains(n))
        .ok_or("interval must be 1s through 1h")?;
    Ok(std::time::Duration::from_millis(millis))
}
impl Cli {
    pub fn parse_cli() -> Self {
        // Global report flags still parse before/after subcommands, but do not
        // advertise historical ranges on the today-only monitor.
        let mut command = Self::command().mut_subcommand("monitor", |monitor| {
            monitor.defer(|mut monitor| {
                for name in ["today", "all", "since", "until", "last"] {
                    monitor = monitor.mut_arg(name, |arg| arg.hide(true));
                }
                monitor
            })
        });
        let matches = command.get_matches_mut();
        let mut cli = Self::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
        cli.explicit = matches
            .ids()
            .filter(|id| {
                matches.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine)
            })
            .map(|id| id.to_string())
            .collect();
        if cli.has_monitor_range() {
            clap::Error::raw(clap::error::ErrorKind::ArgumentConflict,"monitor summarizes today only; use a report command such as daily for --last, --all, --since, or --until").exit();
        }
        cli
    }
    pub fn has_monitor_range(&self) -> bool {
        self.monitor()
            && (self.all || self.since.is_some() || self.until.is_some() || self.last.is_some())
    }
    pub fn monitor(&self) -> bool {
        matches!(self.command, Some(Command::Monitor { .. }))
    }
    pub fn interval(&self) -> std::time::Duration {
        if let Some(Command::Monitor { interval, .. }) = self.command {
            interval
        } else {
            std::time::Duration::from_secs(2)
        }
    }
    pub fn frames(&self) -> Option<u64> {
        if let Some(Command::Monitor { frames, .. }) = self.command {
            frames
        } else {
            None
        }
    }
    pub fn groups(&self) -> Vec<Group> {
        if !self.group_by.is_empty() {
            return self.group_by.clone();
        }
        match self.command {
            Some(Command::Daily) => vec![Group::Day, Group::Model, Group::Mode],
            Some(Command::Monthly) => vec![Group::Month, Group::Model, Group::Mode],
            Some(Command::Sessions | Command::Session { .. }) => {
                vec![Group::Session, Group::Model, Group::Mode]
            }
            _ => vec![Group::Model, Group::Mode],
        }
    }
    pub fn kind(&self) -> &'static str {
        match self.command {
            Some(Command::Daily) => "Daily",
            Some(Command::Monthly) => "Monthly",
            Some(Command::Sessions) => "Sessions",
            Some(Command::Session { .. }) => "Session",
            Some(Command::Monitor { .. }) => "Monitor",
            _ => "Models",
        }
    }
}
