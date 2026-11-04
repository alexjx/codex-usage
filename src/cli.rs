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
    after_help = "Defaults:\n  No command repeats your last successful report, including its filters.\n  Explicit report commands start with this month. Monitor shows today only.\n  Costs estimate API model tokens in USD, not subscription charges.\n\nExamples:\n  codex-usage daily --last 7\n  codex-usage models --today --mode fast\n  codex-usage monitor --source wsl --source windows\n  codex-usage sources\n\nPress Ctrl+C to cancel loading; use q or Ctrl+C to leave the live viewer."
)]
pub struct Cli {
    #[arg(skip)]
    #[serde(skip)]
    pub explicit: Vec<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Config file; otherwise CODEX_USAGE_CONFIG or the native app config directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Select named source; repeat to combine Windows and WSL.
    #[arg(
        long = "source",
        global = true,
        conflicts_with = "roots",
        value_name = "NAME"
    )]
    pub sources: Vec<String>,
    /// Read a Codex home directly; repeat to combine paths, or use with sources set.
    #[arg(long = "root", global = true, value_name = "PATH")]
    pub roots: Vec<PathBuf>,
    /// Include an exact model ID; repeat to include several models.
    #[arg(long = "model", global = true, value_name = "ID")]
    pub models: Vec<String>,
    /// Filter recorded service mode; missing historical settings remain unknown.
    #[arg(long, global = true)]
    pub mode: Option<Mode>,
    /// Include a session ID; repeat to include several sessions.
    #[arg(long = "session", global = true, value_name = "ID")]
    pub sessions: Vec<String>,
    /// Match project path or directory name.
    #[arg(long = "project", global = true, value_name = "PATH_OR_NAME")]
    pub projects: Vec<String>,
    /// Show today's usage in the selected timezone.
    #[arg(long, global = true, conflicts_with_all = ["all", "since", "until", "last"])]
    pub today: bool,
    /// Include all retained usage, with no date limit.
    #[arg(long, global = true, conflicts_with_all = ["today", "since", "until", "last"])]
    pub all: bool,
    /// Include usage from this date onward (inclusive, YYYY-MM-DD).
    #[arg(long, global = true, value_name = "DATE")]
    pub since: Option<NaiveDate>,
    /// Include usage through this date (inclusive, YYYY-MM-DD).
    #[arg(long, global = true, value_name = "DATE")]
    pub until: Option<NaiveDate>,
    /// Report the last N calendar days, including today.
    #[arg(long, global=true, value_name="N", value_parser=clap::value_parser!(u32).range(1..), conflicts_with_all=["today","all","since","until"])]
    pub last: Option<u32>,
    /// Timezone for date boundaries, e.g. Asia/Shanghai (default: config or host).
    #[arg(long, global = true, value_name = "ZONE")]
    pub timezone: Option<String>,
    /// Override report grouping with comma-separated fields, e.g. day,model,mode.
    #[arg(long, global = true, value_delimiter = ',', value_name = "FIELDS")]
    pub group_by: Vec<Group>,
    /// Sort rows (default: total for reports, model for monitor).
    #[arg(long, global = true, value_name = "FIELD")]
    pub sort: Option<Sort>,
    /// Output format: table/json/csv for reports; table/jsonl for monitor.
    #[arg(long, global = true)]
    pub format: Option<Format>,
    /// When to emit ANSI colors (auto respects NO_COLOR and redirected output).
    #[arg(long, global = true, default_value = "auto")]
    pub color: Color,
    /// Color palette for your terminal background.
    #[arg(long, global = true, default_value = "terminal")]
    pub theme: Theme,
    /// Scrollable report viewer; auto opens it when a table exceeds the screen.
    #[arg(long, global = true, default_value = "auto")]
    pub pager: Pager,
    /// Use ASCII separators in report headings.
    #[arg(long, global = true)]
    pub ascii: bool,
    /// Print full token counts instead of K/M/B abbreviations.
    #[arg(long, global = true)]
    pub exact: bool,
    /// Include reasoning, cache-write, and session-count columns.
    #[arg(long, global = true)]
    pub wide: bool,
    /// Skip cost estimation and hide cost columns.
    #[arg(long, global = true)]
    pub no_cost: bool,
    /// Fail on incomplete data or, when costs are enabled, unpriced/assumed costs.
    #[arg(long, global = true)]
    pub strict: bool,
    /// TOML pricing overrides; unmatched requests use bundled rates.
    #[arg(long, global = true, value_name = "PATH")]
    pub pricing_file: Option<PathBuf>,
    /// Use rates for this date instead of the current snapshot (YYYY-MM-DD).
    #[arg(long, global = true, value_name = "DATE")]
    pub pricing_date: Option<NaiveDate>,
}
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum Command {
    /// Summarize usage by model and recorded service mode.
    #[command(
        after_help = "Examples:\n  codex-usage models --today\n  codex-usage models --last 7 --mode fast\n  codex-usage models --all --format json\n\nDefault period: this month. Use --group-by model to combine service modes."
    )]
    Models,
    /// Report daily usage, with model and mode rows under each date.
    #[command(
        after_help = "Examples:\n  codex-usage daily --last 7\n  codex-usage daily --since 2026-11-01 --until 2026-11-04\n  codex-usage daily --all --format csv\n\nDefault period: this month. --last N includes today; date bounds are inclusive."
    )]
    Daily,
    /// Report monthly usage by model and recorded service mode.
    #[command(
        after_help = "Examples:\n  codex-usage monthly --all\n  codex-usage monthly --since 2026-01-01\n\nDefault period: this month. Use --all or --since to include earlier months."
    )]
    Monthly,
    /// Break down each session by model and recorded service mode.
    #[command(
        after_help = "Examples:\n  codex-usage sessions --last 7\n  codex-usage sessions --all --project my-project\n\nDefault period: this month. A session can have several model/mode rows."
    )]
    Sessions,
    /// Report usage for one session ID.
    #[command(
        after_help = "Examples:\n  codex-usage session SESSION_ID --all\n  codex-usage session SESSION_ID --last 7\n\nDefault period: this month. Use --all for the session's complete retained history."
    )]
    Session {
        /// Session ID shown by the sessions report.
        id: String,
    },
    /// Live summary of today's usage in the selected timezone.
    #[command(
        after_help = "Examples:\n  codex-usage monitor\n  codex-usage monitor --source wsl --source windows\n  codex-usage monitor --interval 5s\n  codex-usage monitor --format jsonl --frames 5 > live.jsonl\n\nToday's estimate starts at local midnight; its range ends at the latest snapshot.\nThe title shows the timezone. FILTERED marks active model/mode/session/project filters.\nThe activity row follows --interval (default 2s), showing changes since the previous snapshot.\nn/a means unavailable cost; * marks incomplete estimates.\nTerminal output updates in place; redirected output defaults to JSONL.\nq/Ctrl+C quits; Ctrl+C also cancels loading. Totals reset at local midnight.\nCurrent speed uses actual elapsed time; the parentheses show the 30s per-session average."
    )]
    Monitor {
        /// Refresh cadence, e.g. 2s or 1000ms (1s to 3600s).
        #[arg(long, default_value = "2s", value_parser = parse_interval)]
        interval: std::time::Duration,
        /// Stop after N snapshots, including the initial baseline.
        #[arg(long, value_name="N", value_parser = clap::value_parser!(u64).range(1..))]
        frames: Option<u64>,
    },
    /// List configured/discovered sources, or change their paths and enabled state.
    #[command(
        after_help = "Examples:\n  codex-usage sources\n  codex-usage sources --format json\n  codex-usage sources set windows --kind windows --root /mnt/c/Users/NAME/.codex\n  codex-usage sources set wsl --kind wsl --root ~/.codex\n  codex-usage sources enable windows\n\nWith no action, lists saved sources and accessible candidates. Use the listed name\nto enable a discovered source. Windows and WSL paths are configured independently."
    )]
    Sources {
        #[command(subcommand)]
        action: Option<SourceCommand>,
    },
    /// Show the bundled pricing snapshot and any custom rate overrides.
    #[command(
        after_help = "Examples:\n  codex-usage pricing\n  codex-usage pricing --format json\n  codex-usage pricing --pricing-file rates.toml\n\nRates are USD per million tokens. Reports use current rates unless --pricing-date\nis set; estimates exclude tool fees, regional uplifts, taxes and subscriptions."
    )]
    Pricing,
    /// Check source readability, parsing issues, and duplicate-record coverage.
    #[command(
        after_help = "Examples:\n  codex-usage doctor --no-cost\n  codex-usage doctor --all --strict --no-cost\n  codex-usage doctor --source windows --format json\n\nUse --all to inspect every retained log. --no-cost checks data without requiring\nprice coverage. Report accounting details are also available with ? in the viewer."
    )]
    Doctor,
}
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum SourceCommand {
    /// Add or update one source using --root PATH.
    #[command(
        after_help = "Examples:\n  codex-usage sources set windows --kind windows --root /mnt/c/Users/NAME/.codex\n  codex-usage sources set wsl --kind wsl --root ~/.codex\n\nRequires exactly one --root. A new source requires --kind and starts enabled;\nupdating an existing source preserves its enabled state."
    )]
    Set {
        /// Name to create or update, e.g. windows or wsl.
        name: String,
        /// Source platform; required for a new source, otherwise preserved.
        #[arg(long, value_parser = ["windows", "wsl", "linux"])]
        kind: Option<String>,
    },
    /// Include a saved or discovered source in default reports.
    #[command(
        after_help = "Example:\n  codex-usage sources enable windows\n\nUse a name shown by codex-usage sources. Enabling a discovered candidate saves it\nwithout dropping the current native source."
    )]
    Enable {
        /// Saved source or discovered candidate name shown by sources.
        name: String,
    },
    /// Exclude a source from default reports while keeping its saved path.
    #[command(
        after_help = "Example:\n  codex-usage sources disable windows\n\nThe source remains configured. Select it explicitly with --source NAME to read it\nwithout enabling it by default."
    )]
    Disable {
        /// Source name shown by sources.
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

fn setup_help(mut command: clap::Command, visible: &[&str]) -> clap::Command {
    // Globals still parse as before; show only the options this action uses.
    let globals: Vec<_> = command
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .map(|arg| arg.get_id().to_string())
        .collect();
    for id in globals {
        let hide = !visible.contains(&id.as_str());
        command = command.mut_arg(id, |arg| arg.hide(hide));
    }
    if visible.contains(&"format") {
        command = command.mut_arg("format", |arg| {
            arg.help("Output format: table or json")
                .hide_possible_values(true)
        });
    }
    command
}
impl Cli {
    pub fn parse_cli() -> Self {
        // Global report flags still parse before/after subcommands, but do not
        // advertise historical ranges on the today-only monitor.
        let mut command = Self::command()
            .mut_subcommand("monitor", |monitor| {
                monitor.defer(|mut monitor| {
                    for name in ["today", "all", "since", "until", "last"] {
                        monitor = monitor.mut_arg(name, |arg| arg.hide(true));
                    }
                    monitor.mut_arg("format", |arg| {
                        arg.help(
                            "table or jsonl (default: table in a terminal, jsonl when redirected)",
                        )
                        .hide_possible_values(true)
                    })
                })
            })
            .mut_subcommand("sources", |sources| {
                let sources = sources
                    .mut_subcommand("set", |set| {
                        set.defer(|set| {
                            setup_help(set, &["config", "roots"]).mut_arg("roots", |arg| {
                                arg.help(
                                    "Codex home directory to save; exactly one path is required",
                                )
                            })
                        })
                    })
                    .mut_subcommand("enable", |enable| {
                        enable.defer(|enable| setup_help(enable, &["config"]))
                    })
                    .mut_subcommand("disable", |disable| {
                        disable.defer(|disable| setup_help(disable, &["config"]))
                    });
                sources.defer(|sources| setup_help(sources, &["config", "format"]))
            })
            .mut_subcommand("pricing", |pricing| {
                pricing.defer(|pricing| {
                    setup_help(
                        pricing,
                        &["config", "format", "pricing_file", "pricing_date"],
                    )
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
