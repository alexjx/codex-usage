use crate::model::AgentKind;
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
    Agent,
    Provider,
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
    about = "Local Codex and Claude Code usage reports and live monitoring",
    after_help = "Defaults:\n  With no subcommand, run daily since the most recent Codex quota reset when every\n  selected source is Codex. Any Claude Code source uses the last 7 days, including today.\n  Every invocation uses fresh command options. Monitor shows today only.\n  Costs estimate API model tokens in USD, not subscription charges.\n\nExamples:\n  agent-usage daily --last 7\n  agent-usage models --today --mode fast\n  agent-usage models --agent claude-code --all\n  agent-usage monitor --source codex --source claude\n  agent-usage sources\n\nPress Ctrl+C to cancel loading; use q or Ctrl+C to leave the live viewer."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Config file; otherwise AGENT_USAGE_CONFIG or the native app config directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Select agent; repeat to combine Codex and Claude Code.
    #[arg(long = "agent", global = true, value_name = "AGENT")]
    pub agents: Vec<AgentKind>,
    /// Read this session's conversation, including messages and tool activity.
    #[arg(long, global = true, hide = true)]
    #[serde(default)]
    pub conversation: bool,
    /// Select named source; repeat to combine Windows and WSL.
    #[arg(
        long = "source",
        global = true,
        conflicts_with = "roots",
        value_name = "NAME"
    )]
    pub sources: Vec<String>,
    /// Read an agent data root directly; repeat to combine paths. Requires --agent.
    #[arg(long = "root", global = true, value_name = "PATH")]
    pub roots: Vec<PathBuf>,
    /// Include an exact model ID; repeat to include several models.
    #[arg(long = "model", global = true, value_name = "ID")]
    pub models: Vec<String>,
    /// Filter recorded service mode; missing historical settings remain unknown.
    #[arg(long, global = true)]
    pub mode: Option<Mode>,
    /// Include a session selector; repeat to include several sessions.
    #[arg(long = "session", global = true, value_name = "SELECTOR")]
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
    /// Use ASCII separators and chart bars.
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
        after_help = "Examples:\n  agent-usage models --today\n  agent-usage models --last 7 --mode fast\n  agent-usage models --all --format json\n\nDefault period: this month. Use --group-by model to combine service modes."
    )]
    Models,
    /// Report daily usage, with model and mode rows under each date.
    #[command(
        after_help = "Examples:\n  agent-usage daily --last 7\n  agent-usage daily --since 2026-11-01 --until 2026-11-04\n  agent-usage daily --all --format csv\n  agent-usage daily --agent claude-code --last 7\n\nDefault period: Codex-only selections use the most recent quota reset,\nincluding short-term quotas. Any Claude Code source uses the last 7 days\nbecause Claude quota data is unsupported. Explicit dates always override this.\nNo extra history is loaded to find a reset. --last N includes today; date bounds are inclusive."
    )]
    Daily,
    /// Report monthly usage by model and recorded service mode.
    #[command(
        after_help = "Examples:\n  agent-usage monthly --all\n  agent-usage monthly --since 2026-01-01\n\nDefault period: this month. Use --all or --since to include earlier months."
    )]
    Monthly,
    /// Break down each session by model and recorded service mode.
    #[command(
        after_help = "Examples:\n  agent-usage sessions --last 7\n  agent-usage sessions --all --project my-project\n\nDefault period: this month. A session can have several model/mode rows."
    )]
    Sessions,
    /// Report usage for one session selector.
    #[command(
        after_help = "Examples:\n  agent-usage session codex:SESSION_ID --all\n  agent-usage session claude-code:SESSION_ID --last 7\n  agent-usage session claude-code:SESSION_ID:AGENT_ID --conversation\n  agent-usage session claude-code:SESSION_ID --conversation --format json\n\nSelectors are <agent>:<session>[:<execution>]; reserved bytes are percent-encoded.\nA bare session ID resolves only when exactly one session matches.\nDefault period: this month. Use --all for the session's complete retained history."
    )]
    Session {
        /// Qualified session selector, or a unique bare session ID.
        id: String,
    },
    /// Live summary of today's usage in the selected timezone.
    #[command(
        after_help = "Examples:\n  agent-usage monitor\n  agent-usage monitor --source codex --source claude\n  agent-usage monitor --agent claude-code\n  agent-usage monitor --interval 5s\n  agent-usage monitor --format jsonl --frames 5 > live.jsonl\n\nToday's estimate starts at local midnight; its range ends at the latest snapshot.\nThe title shows the timezone. FILTERED marks active model/mode/session/project filters.\nEach selected agent has its own activity, 2s rolling output chart, and chart history;\nagents are never averaged together. Model rows are grouped under Codex and Claude headings, with plain model names.\nA shared Interval heading introduces indented per-agent deltas and speeds.\nThe activity row follows --interval (default 2s), showing changes since the previous snapshot.\nn/a means unavailable cost; * marks incomplete estimates.\nFullscreen: summary, per-agent charts (up to 180 points each), model table, pinned total/controls.\nCharts open with recent logged history, then update each refresh.\nQuota status and approximate remaining total tokens/exhaustion appear for Codex when loaded logs support them.\nNo extra files are loaded for quota forecasts; unreliable forecasts are omitted.\nc toggles the charts; they hide automatically when space is limited.\nTerminal output updates in place; redirected output defaults to JSONL.\nq/Ctrl+C quits; Ctrl+C also cancels loading. Totals and charts reset at local midnight.\nCurrent speed uses actual elapsed time; the parentheses show the 60s per-session average."
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
        after_help = "Examples:\n  agent-usage sources\n  agent-usage sources --format json\n  agent-usage sources --agent claude-code\n  agent-usage sources set codex --agent codex --kind wsl --root ~/.codex\n  agent-usage sources set claude --agent claude-code --kind wsl --root ~/.claude\n  agent-usage sources enable windows-USER-claude-code\n\nWith no action, lists saved sources and accessible candidates with agent, OS,\nenabled state, and path availability. A new source requires --agent and --kind.\nUse the listed name to enable a discovered source; enabling materializes all\ncurrent implicit native sources first. Paths are configured independently."
    )]
    Sources {
        #[command(subcommand)]
        action: Option<SourceCommand>,
    },
    /// Show the bundled pricing snapshot and any custom rate overrides.
    #[command(
        after_help = "Examples:\n  agent-usage pricing\n  agent-usage pricing --format json\n  agent-usage pricing --pricing-file rates.toml\n\nRates are USD per million tokens for OpenAI, Anthropic, and common third-party\nAnthropic-compatible models. Cache writes are priced per TTL bucket; a missing\nrate leaves the event unpriced rather than silently discounted. Reports use\ncurrent rates unless --pricing-date is set; estimates exclude tool fees,\nregional uplifts, taxes and subscriptions."
    )]
    Pricing,
    /// Check source readability, parsing issues, and duplicate-record coverage.
    #[command(
        after_help = "Examples:\n  agent-usage doctor --no-cost\n  agent-usage doctor --agent claude-code --no-cost\n  agent-usage doctor --all --strict --no-cost\n  agent-usage doctor --source claude-linux --format json\n\nUse --all to inspect every retained log. Each source reports its agent, quota\ncapability, local coverage, and excluded usage. --no-cost checks data without\nrequiring price coverage. Report accounting details are also available with ? in the viewer."
    )]
    Doctor,
}
#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum SourceCommand {
    /// Add or update one source using --root PATH.
    #[command(
        after_help = "Examples:\n  agent-usage sources set codex --agent codex --kind wsl --root ~/.codex\n  agent-usage sources set claude --agent claude-code --kind linux --root ~/.claude\n\nRequires exactly one --root. A new source requires --agent and --kind and starts\nenabled; updating an existing source preserves omitted agent/kind and its enabled state."
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
        after_help = "Example:\n  agent-usage sources enable windows\n\nUse a name shown by agent-usage sources. Enabling a discovered candidate saves it\nwithout dropping the current native source."
    )]
    Enable {
        /// Saved source or discovered candidate name shown by sources.
        name: String,
    },
    /// Exclude a source from default reports while keeping its saved path.
    #[command(
        after_help = "Example:\n  agent-usage sources disable windows\n\nThe source remains configured. Select it explicitly with --source NAME to read it\nwithout enabling it by default."
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
            .mut_subcommand("session", |session| {
                session.defer(|session| session.mut_arg("conversation", |arg| arg.hide(false)))
            })
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
        cli.command.get_or_insert(Command::Daily);
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
    pub fn default_daily_period(&self) -> bool {
        matches!(self.command, None | Some(Command::Daily))
            && !self.today
            && !self.all
            && self.since.is_none()
            && self.until.is_none()
            && self.last.is_none()
    }
    pub fn conversation(&self) -> bool {
        self.conversation && matches!(self.command, Some(Command::Session { .. }))
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
            None | Some(Command::Daily) => vec![
                Group::Day,
                Group::Agent,
                Group::Provider,
                Group::Model,
                Group::Mode,
            ],
            Some(Command::Monthly) => vec![
                Group::Month,
                Group::Agent,
                Group::Provider,
                Group::Model,
                Group::Mode,
            ],
            Some(Command::Sessions | Command::Session { .. }) => vec![
                Group::Session,
                Group::Agent,
                Group::Provider,
                Group::Model,
                Group::Mode,
            ],
            _ => vec![Group::Agent, Group::Provider, Group::Model, Group::Mode],
        }
    }
    pub fn kind(&self) -> &'static str {
        match self.command {
            None | Some(Command::Daily) => "Daily",
            Some(Command::Monthly) => "Monthly",
            Some(Command::Sessions) => "Sessions",
            Some(Command::Session { .. }) => "Session",
            Some(Command::Monitor { .. }) => "Monitor",
            _ => "Models",
        }
    }
}
