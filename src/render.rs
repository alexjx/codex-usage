use crate::{
    cli::{Cli, Color, Theme},
    report::{Counts, Report},
};
use rust_decimal::{Decimal, RoundingStrategy};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn agent_name(agent: &str) -> &str {
    match agent {
        "codex" => "Codex",
        "claude-code" => "Claude",
        _ => agent,
    }
}
fn row_label(row: &crate::report::Row, grouped: bool) -> String {
    if !grouped {
        return row.label();
    }
    let mut display = row.clone();
    display.dimensions.remove("agent");
    display.dimensions.remove("provider");
    format!(
        "{}{}",
        if row.dimensions.contains_key("day") || row.dimensions.contains_key("month") {
            "    "
        } else {
            "  "
        },
        display.label()
    )
}

pub struct Style {
    color: bool,
    theme: Theme,
}
impl Style {
    pub fn new(cli: &Cli, tty: bool) -> Self {
        Self {
            color: match cli.color {
                Color::Always => true,
                Color::Never => false,
                Color::Auto => {
                    tty && std::env::var_os("NO_COLOR").is_none()
                        && std::env::var("TERM").unwrap_or_default() != "dumb"
                }
            },
            theme: cli.theme,
        }
    }
    pub fn paint(&self, role: &str, value: impl AsRef<str>) -> String {
        let value = value.as_ref();
        if !self.color {
            return value.into();
        }
        let code = match (self.theme, role) {
            (_, "bold") => "1",
            (Theme::Dark, "date") => "1;38;2;230;237;247;48;2;44;55;73",
            (Theme::Light, "date") => "1;38;2;30;45;65;48;2;228;234;242",
            (Theme::Dark, "codex") => "1;38;2;125;190;245",
            (Theme::Light, "codex") => "1;38;2;30;91;151",
            (Theme::Dark, "claude") => "1;38;2;224;173;132",
            (Theme::Light, "claude") => "1;38;2;148;79;37",
            (_, "date") => "1;7",
            (_, "codex") => "1;34",
            (_, "claude") => "1;33",

            (Theme::Dark, "subtotal") => "38;2;146;183;191",
            (Theme::Dark, "subtotal_cost") => "38;2;172;159;206",
            (Theme::Dark, "total") => "1;38;2;103;212;232",
            (Theme::Dark, "total_cost") => "1;38;2;196;181;253",
            (Theme::Light, "subtotal") => "38;2;82;111;120",
            (Theme::Light, "subtotal_cost") => "38;2;112;100;139",
            (Theme::Light, "total") => "1;38;2;0;103;125",
            (Theme::Light, "total_cost") => "1;38;2;107;63;160",
            (_, "subtotal") => "36",
            (_, "subtotal_cost") => "35",
            (_, "total") => "1;96",
            (_, "total_cost") => "1;95",
            (Theme::Dark, "accent") => "38;2;103;212;232",
            (Theme::Dark, "cost") => "38;2;196;181;253",
            (Theme::Dark, "ok") => "38;2;134;211;165",
            (Theme::Dark, "warn") => "38;2;240;198;116",
            (Theme::Dark, "meta") => "38;2;168;179;199",
            (Theme::Light, "accent") => "38;2;0;103;125",
            (Theme::Light, "cost") => "38;2;107;63;160",
            (Theme::Light, "ok") => "38;2;23;100;61",
            (Theme::Light, "warn") => "38;2;128;86;0",
            (Theme::Light, "meta") => "38;2;82;96;117",
            (_, "accent") => "36",
            (_, "cost") => "35",
            (_, "ok") => "32",
            (_, "warn") => "33",
            _ => "0",
        };
        format!("\x1b[{code}m{value}\x1b[0m")
    }
}
pub fn integer(n: u64) -> String {
    let raw = n.to_string();
    let mut out = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
pub fn number(n: u64, exact: bool) -> String {
    if exact || n < 10000 {
        return integer(n);
    }
    let (scale, suffix) = if n >= 1_000_000_000 {
        (1_000_000_000, "B")
    } else if n >= 1_000_000 {
        (1_000_000, "M")
    } else {
        (1000, "K")
    };
    format!("{:.1}{suffix}", n as f64 / scale as f64)
}
pub fn money(n: Option<Decimal>) -> String {
    match n {
        None => "n/a".into(),
        Some(d) if d > Decimal::ZERO && d < Decimal::new(1, 4) => "<$0.0001".into(),
        Some(d) => format!(
            "${:.4}",
            d.round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero)
        ),
    }
}
fn includes_claude(report: &Report) -> bool {
    report
        .sources
        .iter()
        .any(|source| source.agent == crate::model::AgentKind::ClaudeCode)
}
fn quota_lines(cli: &Cli, report: &Report, sep: &str) -> Vec<(bool, String)> {
    if includes_claude(report) {
        return Vec::new();
    }
    let tz = report
        .period
        .timezone
        .parse::<chrono_tz::Tz>()
        .unwrap_or(chrono_tz::UTC);
    let date = |time: chrono::DateTime<chrono::Utc>| {
        time.with_timezone(&tz).format("%m-%d %H:%M").to_string()
    };
    // Share display headings only. Sources retain their own values and forecasts.
    let mut groups = std::collections::BTreeMap::new();
    for quota in &report.quotas {
        groups
            .entry((
                quota.limit_id.as_str(),
                quota.window_minutes,
                quota.next_reset.timestamp().div_euclid(60),
            ))
            .or_insert_with(Vec::new)
            .push(quota);
    }
    groups
        .into_iter()
        .map(|((limit, minutes, _), quotas)| {
            let reset = date(quotas[0].next_reset);
            let window = if minutes.is_multiple_of(1440) {
                format!("{}d", minutes / 1440)
            } else if minutes.is_multiple_of(60) {
                format!("{}h", minutes / 60)
            } else {
                format!("{minutes}m")
            };
            let label = if limit == "codex" {
                window
            } else {
                format!("{limit}/{window}")
            };
            let expired = quotas.iter().all(|q| q.next_reset <= report.generated_at);
            let estimated = quotas.iter().any(|q| q.status != "current");
            let mut line = if expired {
                format!("Quota {label}{sep}reset unavailable")
            } else {
                format!(
                    "Quota {label}{sep}reset {}{reset}",
                    if estimated { "~" } else { "" }
                )
            };
            let mut warning = expired;
            for quota in &quotas {
                // Status diagnostics belong in help/JSON, not the main screen.
                if quota.status != "current" || quota.remaining_tokens.is_none() {
                    continue;
                }
                let source = if quotas.iter().filter(|q| q.source == quota.source).count() > 1 {
                    format!("{}/{}", quota.source, quota.bucket)
                } else {
                    quota.source.clone()
                };
                warning |= quota.used_percent >= 90.0;
                let percentage = format!("{:.1}", quota.used_percent);
                let percentage = percentage.strip_suffix(".0").unwrap_or(&percentage);
                line.push_str(&format!("{sep}{source} {percentage}%"));
                if let Some(tokens) = quota.remaining_tokens {
                    line.push_str(&format!(" ~{} tok left", number(tokens, cli.exact)));
                }
                if let Some(time) = quota.exhausts_at {
                    line.push_str(&format!(" (exhaust ~{})", date(time)));
                } else if quota.forecast_status == "resets_first" {
                    line.push_str(" (resets first)");
                }
            }
            (warning, line)
        })
        .collect()
}
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if cells + w > width.max(1) && !line.is_empty() {
            out.push(std::mem::take(&mut line));
            cells = 0;
        }
        if c != '\x1b' && !c.is_control() {
            line.push(c);
            cells += w;
        }
    }
    out.push(line);
    out
}
fn text(lines: &mut Vec<String>, style: &Style, role: &str, raw: &str, width: usize) {
    lines.extend(wrap(raw, width).iter().map(|s| style.paint(role, s)));
}
fn pad(value: &str, width: usize, right: bool) -> String {
    let value: String = value.chars().filter(|c| !c.is_control()).collect();
    let gap = " ".repeat(width.saturating_sub(value.width()));
    if right {
        format!("{gap}{value}")
    } else {
        format!("{value}{gap}")
    }
}
fn subset(value: u64, missing: usize, events: usize, cli: &Cli) -> String {
    if events > 0 && missing == events {
        "n/a".into()
    } else {
        format!(
            "{}{}",
            number(value, cli.exact),
            if missing > 0 { "*" } else { "" }
        )
    }
}
fn values(counts: &Counts, cli: &Cli, detailed: bool) -> Vec<String> {
    let mut out = vec![mode_label(counts).to_string()];
    if detailed {
        out.extend([
            number(counts.input, cli.exact),
            subset(counts.cached, counts.cached_missing, counts.events, cli),
            number(counts.output, cli.exact),
        ]);
    }
    if cli.wide && detailed {
        out.push(subset(
            counts.reasoning,
            counts.reasoning_missing,
            counts.events,
            cli,
        ));
        out.push(subset(
            counts.cache_write,
            counts.cache_write_missing,
            counts.events,
            cli,
        ));
        out.push(counts.sessions.to_string());
    }
    out.push(number(counts.total, cli.exact));
    if !cli.no_cost {
        out.push(format!(
            "{}{}",
            money(counts.cost_usd),
            if counts.priced_tokens > 0 && counts.unpriced_tokens > 0 {
                "*"
            } else {
                ""
            }
        ));
    }
    out
}
fn mode_label(counts: &Counts) -> &str {
    if counts.modes.len() > 1 {
        return "Mixed";
    }
    match counts.modes.first().map(String::as_str) {
        Some("regular") => "Regular",
        Some("fast") => "Fast",
        Some("flex") => "Flex",
        Some("ultrafast") => "Ultrafast",
        Some("batch") => "Batch",
        _ => "Unknown",
    }
}
fn date_label(key: &str, value: &str) -> String {
    let date = if key == "day" {
        chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
    } else {
        chrono::NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d")
    };
    date.map(|d| {
        d.format(if key == "day" {
            "%a, %b %d %Y"
        } else {
            "%B %Y"
        })
        .to_string()
    })
    .unwrap_or_else(|_| value.into())
}
pub struct View {
    pub header: Vec<String>,
    pub chart_anchor: usize,
    pub body: Vec<String>,
    pub footer: Vec<String>,
    pub identities: Vec<(String, usize)>,
    pub sections: Vec<(String, usize)>,
    pub session_rows: Vec<SessionRow>,
}
pub struct SessionRow {
    pub session: String,
    /// Typed navigation target; never reconstructed from rendered text.
    pub key: Option<crate::model::SessionKey>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub start: usize,
    pub end: usize,
}

struct SummaryLayout<'a> {
    cli: &'a Cli,
    style: &'a Style,
    width: usize,
    id_width: usize,
    stacked: bool,
    titles: &'a [&'a str],
    widths: &'a [usize],
}

fn summary_lines(
    label: &str,
    counts: &Counts,
    cells: &[String],
    layout: &SummaryLayout<'_>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let SummaryLayout {
        cli,
        style,
        width,
        id_width,
        stacked,
        titles,
        widths,
    } = layout;
    let (role, cost_role) = if label.trim() == "Subtotal" {
        ("subtotal", "subtotal_cost")
    } else {
        ("total", "total_cost")
    };
    if *stacked {
        text(&mut lines, style, role, label, *width);
        let detail = if cli.wide {
            titles
                .iter()
                .zip(cells)
                .skip(1)
                .map(|(label, value)| format!("{label} {value}"))
                .collect::<Vec<_>>()
                .join(" / ")
        } else {
            let mut detail = format!("{} tokens", number(counts.total, cli.exact));
            if !cli.no_cost {
                detail.push_str(&format!(" / {}", cells.last().unwrap()));
            }
            detail
        };
        text(&mut lines, style, role, &detail, *width);
    } else {
        let mut line = style.paint(role, pad(label, *id_width, false));
        for (index, (value, width)) in cells.iter().zip(*widths).enumerate() {
            line.push_str("  ");
            line.push_str(&style.paint(
                if !cli.no_cost && index + 1 == cells.len() {
                    cost_role
                } else {
                    role
                },
                pad(value, *width, true),
            ));
        }
        lines.push(line);
    }
    lines
}
pub fn view(cli: &Cli, report: &Report, style: &Style, width: usize, help: bool) -> View {
    let mut h = Vec::new();
    let mut b = Vec::new();
    let mut total_footer = Vec::new();
    let mut identities = Vec::new();
    let mut sections = Vec::new();
    let mut session_rows = Vec::new();
    let sep = if cli.ascii { " / " } else { " · " };
    let filtered = cli.monitor()
        && (!cli.models.is_empty()
            || !cli.sessions.is_empty()
            || !cli.projects.is_empty()
            || cli.mode.is_some());
    let incomplete = cli.monitor()
        && (report.coverage.incomplete() || report.sources.iter().any(|source| !source.readable));
    let cost_unavailable = incomplete && report.totals.events == 0;
    let cost_assumed = cli.monitor() && report.totals.assumed_events > 0;
    let summary_cost = if cost_unavailable {
        None
    } else {
        report.totals.cost_usd
    };
    let tz = report
        .period
        .timezone
        .parse::<chrono_tz::Tz>()
        .unwrap_or(chrono_tz::UTC);
    let period_label = match (report.period.start, report.period.basis.as_str()) {
        (Some(start), "last_reset_observed" | "last_reset_estimated") => format!(
            "Since reset {}{}",
            if report.period.basis == "last_reset_estimated" {
                "~"
            } else {
                ""
            },
            start.with_timezone(&tz).format("%m-%d %H:%M")
        ),
        _ => report.period.label.clone(),
    };
    text(
        &mut h,
        style,
        "accent",
        &format!(
            "Agent Usage / {}{}{}{sep}{}{sep}{}",
            report.kind,
            if cli.monitor() { " LIVE" } else { "" },
            if filtered { " FILTERED" } else { "" },
            period_label,
            report.period.timezone
        ),
        width,
    );
    // Style source status independently while keeping paths out of the primary view.
    let mut status = report
        .sources
        .iter()
        .map(|s| format!("{} {}", s.name, if s.readable { "ready" } else { "stale" }))
        .collect::<Vec<_>>()
        .join(sep);
    if incomplete {
        if !status.is_empty() {
            status.push_str(sep);
        }
        status.push_str("partial data");
    }
    text(
        &mut h,
        style,
        if !incomplete && report.sources.iter().all(|s| s.readable) {
            "ok"
        } else {
            "warn"
        },
        &status,
        width,
    );
    let mut summary = String::new();
    if cli.monitor() {
        let range = report
            .period
            .timezone
            .parse::<chrono_tz::Tz>()
            .ok()
            .and_then(|tz| {
                report.period.start.map(|start| {
                    format!(
                        "Today {}{}{}",
                        start.with_timezone(&tz).format("%H:%M:%S"),
                        if cli.ascii { "-" } else { "–" },
                        report.generated_at.with_timezone(&tz).format("%H:%M:%S")
                    )
                })
            })
            .unwrap_or_else(|| "Today".into());
        summary.push_str(&format!("{range}{sep}"));
    }
    summary.push_str(&format!(
        "{} tokens",
        number(report.totals.total, cli.exact)
    ));
    if !cli.no_cost {
        summary.push_str(&format!(
            "{sep}{} est.{}{}",
            money(summary_cost),
            if summary_cost.is_some() && (incomplete || cost_assumed) {
                "*"
            } else {
                ""
            },
            if report.totals.unpriced_tokens > 0 && report.totals.priced_tokens > 0 {
                " subtotal"
            } else {
                ""
            }
        ));
    }
    text(&mut h, style, "cost", &summary, width);
    if report.period.basis.starts_with("last_7_days_") {
        let reason = match report.period.basis.as_str() {
            "last_7_days_conflicting_resets" => "quota resets differ by source",
            "last_7_days_multi_agent" => "multiple agents selected",
            "last_7_days_no_quota_capability" => "quota unavailable for this agent",
            _ => "quota reset unavailable",
        };
        text(
            &mut h,
            style,
            "meta",
            &if includes_claude(report) {
                "Last 7 days".into()
            } else {
                format!("Last 7 days{sep}{reason}")
            },
            width,
        );
    }
    if let Some(m) = &report.monitor {
        h.push(String::new());
        text(
            &mut h,
            style,
            "meta",
            &format!("Interval ({}s):", m.window_seconds),
            width,
        );
        if m.agent_activity.is_empty() {
            let mut delta = format!("  {:+} tokens", m.window_tokens);
            if !cli.no_cost {
                delta.push_str(
                    &m.window_cost_usd
                        .map(|cost| {
                            format!(
                                " / {cost:+.4} USD{}",
                                if m.window_cost_partial { "*" } else { "" }
                            )
                        })
                        .unwrap_or(" / n/a USD".into()),
                );
            }
            delta.push_str(&format!(
                " / {:.0} out tok/s ({:.1} tok/s)",
                m.output_tokens_per_second, m.average_session_output_tokens_per_second
            ));
            text(&mut h, style, "accent", &delta, width);
        }
        let token_width = m
            .agent_activity
            .iter()
            .map(|a| format!("{:+}", a.window_tokens).len())
            .max()
            .unwrap_or(1);
        for agent in &m.agent_activity {
            let agent_id = agent.agent.to_string();
            let name = agent_name(&agent_id);
            let unavailable = !report.sources.is_empty()
                && report
                    .sources
                    .iter()
                    .filter(|s| s.agent == agent.agent)
                    .all(|s| !s.readable);
            let mut metrics = vec![format!("{:+token_width$} tokens", agent.window_tokens)];
            if !cli.no_cost {
                metrics.push(
                    agent
                        .window_cost_usd
                        .filter(|_| {
                            !unavailable && !(m.window_cost_usd.is_none() && m.window_cost_partial)
                        })
                        .map(|cost| {
                            format!(
                                "{cost:+.4} USD{}",
                                if agent.cost_partial { "*" } else { "" }
                            )
                        })
                        .unwrap_or("n/a USD".into()),
                );
            }
            metrics.push(if unavailable {
                "n/a out tok/s (n/a tok/s)".into()
            } else {
                format!(
                    "{:.0} out tok/s ({:.1} tok/s)",
                    agent.output_tokens_per_second, agent.average_session_output_tokens_per_second
                )
            });
            let prefix = format!("  {name:<6}  ");
            let available = width.saturating_sub(prefix.len()).max(1);
            for (index, line) in wrap(&metrics.join(" / "), available).iter().enumerate() {
                h.push(style.paint(
                    if agent.agent == crate::model::AgentKind::Codex {
                        "codex"
                    } else {
                        "claude"
                    },
                    format!(
                        "{}{line}",
                        if index == 0 {
                            prefix.clone()
                        } else {
                            " ".repeat(prefix.len())
                        }
                    ),
                ));
            }
        }
    }
    for (warning, line) in quota_lines(cli, report, sep) {
        text(
            &mut h,
            style,
            if warning { "warn" } else { "meta" },
            &line,
            width,
        );
    }
    let chart_anchor = h.len();
    let agent_sections = report
        .rows
        .iter()
        .all(|r| r.dimensions.contains_key("agent"));
    let labels = if agent_sections && !help {
        vec!["Model"]
    } else if help {
        vec!["Detail"]
    } else {
        vec![if cli.groups().iter().all(|g| {
            matches!(
                g,
                crate::cli::Group::Model
                    | crate::cli::Group::Mode
                    | crate::cli::Group::Day
                    | crate::cli::Group::Month
            )
        }) {
            "Model"
        } else {
            "Group"
        }]
    };
    let id_width = report
        .rows
        .iter()
        .map(|r| row_label(r, agent_sections).width())
        .max()
        .unwrap_or(16)
        .max(labels[0].len());
    let id_width = if report.subtotals.is_empty() {
        id_width
    } else {
        id_width.max("  Subtotal".len())
    };
    let mut detailed = cli.wide || width >= id_width + if cli.no_cost { 40 } else { 53 };
    let columns = |detailed: bool| {
        let mut labels = vec!["Mode"];
        if detailed {
            labels.extend(["Input", "Cached", "Output"]);
        }
        if cli.wide && detailed {
            labels.extend(["Reason", "Cache write", "Sessions"]);
        }
        labels.push("Total");
        if !cli.no_cost {
            labels.push("Est. USD");
        }
        labels
    };
    let widths_for = |detailed: bool| {
        let titles = columns(detailed);
        let mut widths: Vec<usize> = titles.iter().map(|s| s.len()).collect();
        for row in report
            .rows
            .iter()
            .map(|r| &r.counts)
            .chain(report.subtotals.iter().map(|row| &row.counts))
            .chain(std::iter::once(&report.totals))
        {
            for (i, v) in values(row, cli, detailed).iter().enumerate() {
                widths[i] = widths[i].max(v.width());
            }
        }
        widths
    };
    let mut widths = widths_for(detailed);
    if !cli.wide && id_width + widths.iter().sum::<usize>() + widths.len() * 2 > width {
        detailed = false;
        widths = widths_for(false);
    }
    let stacked = id_width + widths.iter().sum::<usize>() + widths.len() * 2 > width;
    h.push(String::new());
    let titles = columns(detailed);
    let summary_layout = SummaryLayout {
        cli,
        style,
        width,
        id_width,
        stacked,
        titles: &titles,
        widths: &widths,
    };
    if help {
        text(&mut h, style, "meta", "Help", width);
        let mut details=vec!["Arrows/j/k Scroll | PgUp/PgDn Page | Home/End | q Quit | ? Back".into(),
            "Summary covers the full selection. Cached is part of input; reasoning is part of output. * marks incomplete coverage or assumed estimates; n/a means unavailable.".into(),
            "Speed: newly observed output tokens since the previous snapshot divided by actual elapsed seconds; delayed backlog and corrections are excluded.".into(),
            "Token/cost deltas: changes since the previous snapshot, including corrections and recovered records. The prefix follows --interval (default 2s). Cost uses the known priced subtotal. Idle refreshes show zero; today's summary stays cumulative. Slow collection can extend the actual sample time, available as sample_seconds in JSONL.".into(),
            "Speed in parentheses: mean output rate per active session over 60s (shorter since startup). Idle sessions drop out. Logs arrive in batches; this is observed throughput, not streaming generation speed. Individual rates are available in JSONL.".into(),
            format!("Unpriced: {} tokens",number(report.totals.unpriced_tokens,cli.exact)),
            format!("Data warnings: {} / Pending: {}",report.coverage.malformed+report.coverage.conflicts+report.coverage.uncertain,report.coverage.pending),
            format!("Pricing assumptions: {}",report.totals.assumed_events)];
        if includes_claude(report) {
            details.push("Daily defaults to Last 7 days when Claude is selected. Explicit dates override this.".into());
        } else {
            details.push("Daily defaults to the most recent reset (including short-term quotas) found in already loaded logs. ~ marks an inferred reset; unavailable, expired, or conflicting reset data falls back to seven days. Explicit dates override this. File discovery is never expanded for quotas.".into());
            for quota in &report.quotas {
                details.push(format!("Quota {}/{}/{}: {}% used; reset {}; updated {}; forecast {} (input+output tokens).", quota.source, quota.limit_id, quota.bucket, quota.used_percent, quota.next_reset.with_timezone(&tz).to_rfc3339(), quota.observed_at.with_timezone(&tz).to_rfc3339(), quota.forecast_status));
            }
        }

        if !includes_claude(report) {
            details.push("Quota forecasts cover each source's locally observed workload, independently of report filters. ~ tokens left means estimated input+output tokens, not an entitlement. Forecasts use up to 60 minutes of quota changes, require at least 2 minutes and 2 percentage points, and stop after 5 minutes without an update or when data/mix is unreliable. Usage elsewhere may consume quota without appearing in these logs. Sources are kept separate because logs do not identify the account.".into());
        }
        if matches!(cli.command, Some(crate::cli::Command::Sessions)) {
            details.push("Session rows: arrows or j/k select a row; Enter reads its retained conversation. Esc returns to the same report position. Source selection is preserved; conversation text includes older context outside the report period. Use --pager never for a linear usage report.".into());
        }
        if cli.monitor() {
            details.push("Monitor range: today's local midnight through the displayed snapshot time, in the title's timezone. Cost includes today's loaded history. Stale sources or partial data can omit usage.".into());
            details.push("c toggles the output-speed chart. It shows up to 180 chart points, newest on the right; dots mark zero and ? marks unavailable data. The chart starts with recent logged history and continues with 2s rolling aggregate output (including idle time), sampled each refresh; the parenthesized rate averages per session. The chart hides when space is limited.".into());
        }
        if filtered {
            details.push(format!(
                "Filters: models={:?}; sessions={:?}; projects={:?}; mode={:?}",
                cli.models, cli.sessions, cli.projects, cli.mode
            ));
        }
        if report
            .monitor
            .as_ref()
            .is_some_and(|m| m.window_cost_partial)
        {
            details.push("This refresh includes unpriced usage changes. Its cost is unavailable or a marked known subtotal.".into());
        }
        details.extend(report.totals.assumptions.iter().cloned());
        details.extend(report.totals.pricing_reasons.iter().cloned());
        details.extend(report.coverage.messages.iter().cloned());
        details.extend(report.sources.iter().filter_map(|s| s.error.clone()));
        details.push(format!("Pricing: {}", report.pricing));
        for (i, line) in details.iter().enumerate() {
            identities.push((format!("help-{i}"), b.len()));
            text(&mut b, style, "meta", line, width);
            b.push(String::new());
        }
    } else {
        if !stacked {
            let head = std::iter::once(pad(labels[0], id_width, false))
                .chain(titles.iter().zip(&widths).map(|(s, w)| pad(s, *w, true)))
                .collect::<Vec<_>>()
                .join("  ");
            text(&mut h, style, "meta", &head, width);
        } else {
            text(
                &mut h,
                style,
                "meta",
                if cli.wide {
                    "Group / Usage"
                } else if cli.no_cost {
                    "Group / Mode / Total"
                } else {
                    "Group / Mode / Total / Est. USD"
                },
                width,
            );
        }
        let date_key = if report.rows.iter().any(|r| r.dimensions.contains_key("day")) {
            Some("day")
        } else if report
            .rows
            .iter()
            .any(|r| r.dimensions.contains_key("month"))
        {
            Some("month")
        } else {
            None
        };
        let date_totals: std::collections::BTreeMap<_, _> = report
            .subtotals
            .iter()
            .filter_map(|row| {
                date_key.and_then(|key| {
                    row.dimensions
                        .get(key)
                        .map(|date| (date.as_str(), &row.counts))
                })
            })
            .collect();
        let mut previous_date = None;
        let mut previous_agent = None;
        let mut rows: Vec<_> = report.rows.iter().enumerate().collect();
        let idle_rows: Vec<_> = report
            .sources
            .iter()
            .map(|source| source.agent.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|agent| {
                !report
                    .rows
                    .iter()
                    .any(|row| row.dimensions.get("agent") == Some(agent))
            })
            .map(|agent| crate::report::Row {
                dimensions: [("agent".into(), agent)].into(),
                counts: Counts::default(),
            })
            .collect();
        if agent_sections && cli.monitor() {
            rows.extend(idle_rows.iter().map(|row| (usize::MAX, row)));
        }
        if agent_sections {
            rows.sort_by_key(|(_, row)| {
                (
                    date_key
                        .and_then(|key| row.dimensions.get(key))
                        .cloned()
                        .unwrap_or_default(),
                    match row.dimensions.get("agent").map(String::as_str) {
                        Some("codex") => 0,
                        _ => 1,
                    },
                )
            });
        }
        for (position, &(row_index, row)) in rows.iter().enumerate() {
            if let Some((key, date)) =
                date_key.and_then(|key| row.dimensions.get(key).map(|date| (key, date)))
            {
                if previous_date != Some(date) {
                    if !b.is_empty() {
                        b.push(String::new());
                    }
                    let label = date_label(key, date);
                    identities.push((format!("section:{date}"), b.len()));
                    sections.push((label.clone(), b.len()));
                    text(&mut b, style, "date", &format!(" {label} "), width);
                    previous_date = Some(date);
                    previous_agent = None;
                }
            }

            if agent_sections {
                let agent = row.dimensions.get("agent").unwrap();
                if previous_agent != Some(agent) {
                    if !b.is_empty() && date_key.is_none() {
                        b.push(String::new());
                    }
                    let label = if date_key.is_some() {
                        format!("  {}", agent_name(agent))
                    } else {
                        agent_name(agent).to_owned()
                    };
                    identities.push((format!("agent:{agent}"), b.len()));
                    sections.push((label.clone(), b.len()));
                    text(
                        &mut b,
                        style,
                        if agent == "codex" { "codex" } else { "claude" },
                        &label,
                        width,
                    );
                    previous_agent = Some(agent);
                }
            }
            if row_index == usize::MAX {
                let ready = report.sources.iter().any(|source| {
                    source.agent.to_string() == row.dimensions["agent"] && source.readable
                });
                text(
                    &mut b,
                    style,
                    "meta",
                    if ready {
                        if cli.monitor() {
                            "  No usage today"
                        } else {
                            "  No usage in this period"
                        }
                    } else {
                        "  Usage unavailable"
                    },
                    width,
                );
                continue;
            }
            let row_start = b.len();
            let id = row_label(row, agent_sections);
            identities.push((row.identity(), b.len()));
            if stacked {
                text(&mut b, style, "bold", &id, width);
                let mut detail = if cli.wide {
                    titles
                        .iter()
                        .zip(values(&row.counts, cli, true))
                        .map(|(label, value)| format!("{label} {value}"))
                        .collect::<Vec<_>>()
                        .join(" / ")
                } else {
                    format!(
                        "{} / {} tokens",
                        mode_label(&row.counts),
                        number(row.counts.total, cli.exact)
                    )
                };
                if !cli.no_cost && !cli.wide {
                    detail.push_str(&format!(
                        " / {}{}",
                        money(row.counts.cost_usd),
                        if row.counts.unpriced_tokens > 0 && row.counts.priced_tokens > 0 {
                            "*"
                        } else {
                            ""
                        }
                    ));
                }
                text(&mut b, style, "cost", &detail, width);
                if date_key.is_none() {
                    b.push(String::new());
                }
            } else {
                let vs = values(&row.counts, cli, detailed);
                let mut line = style.paint("bold", pad(&id, id_width, false));
                for (i, (value, w)) in vs.iter().zip(&widths).enumerate() {
                    line.push_str("  ");
                    let role = if i == 0 && mode_label(&row.counts) == "Fast" {
                        "accent"
                    } else if !cli.no_cost && i == vs.len() - 1 {
                        "cost"
                    } else if titles[i] == "Cached" {
                        "meta"
                    } else if titles[i] == "Total" {
                        "bold"
                    } else {
                        "body"
                    };
                    line.push_str(&style.paint(role, pad(value, *w, true)));
                }
                b.push(line);
            }
            if let Some(session) = row.dimensions.get("session") {
                session_rows.push(SessionRow {
                    session: session.clone(),
                    key: session.parse().ok(),
                    model: row.dimensions.get("model").cloned(),
                    mode: row.dimensions.get("mode").cloned().or_else(|| {
                        (row.counts.modes.len() == 1)
                            .then(|| row.counts.modes.first().unwrap().clone())
                    }),
                    start: row_start,
                    end: b.len(),
                });
            }
            if let Some((key, date)) =
                date_key.and_then(|key| row.dimensions.get(key).map(|date| (key, date)))
            {
                let next_date = rows
                    .get(position + 1)
                    .and_then(|(_, row)| row.dimensions.get(key));
                if next_date != Some(date) {
                    if let Some(counts) = date_totals.get(date.as_str()) {
                        let mut cells = values(counts, cli, detailed);
                        cells[0].clear();
                        let subtotal_start = b.len();
                        identities.push((format!("subtotal:{key}:{date}"), b.len()));
                        b.extend(summary_lines("  Subtotal", counts, &cells, &summary_layout));
                        // Selection must scroll the trailing subtotal into view too.
                        if let Some(row) = session_rows.last_mut() {
                            if row.end == subtotal_start {
                                row.end = b.len();
                            }
                        }
                    }
                }
            }
        }
        if report.rows.is_empty() && (incomplete || !agent_sections || report.sources.is_empty()) {
            text(
                &mut b,
                style,
                "meta",
                if incomplete {
                    "Usage data incomplete."
                } else {
                    "No usage in this period."
                },
                width,
            );
        }
        total_footer.push(String::new());
        let mut totals = values(&report.totals, cli, detailed);
        if !cli.no_cost && cli.monitor() {
            if let Some(cost) = totals.last_mut() {
                if cost_unavailable {
                    *cost = "n/a".into();
                } else if summary_cost.is_some()
                    && (incomplete || cost_assumed)
                    && !cost.ends_with('*')
                {
                    cost.push('*');
                }
            }
        }
        // A grand total spans all modes; leave that categorical cell empty.
        totals[0].clear();
        total_footer.extend(summary_lines(
            "Total",
            &report.totals,
            &totals,
            &summary_layout,
        ));
    }
    View {
        header: h,
        chart_anchor,
        body: b,
        footer: total_footer,
        identities,
        sections,
        session_rows,
    }
}

pub fn linear(cli: &Cli, report: &Report, width: usize, tty: bool) -> Vec<String> {
    let v = view(cli, report, &Style::new(cli, tty), width, false);
    v.header.into_iter().chain(v.body).chain(v.footer).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn quota_report() -> (Cli, Report) {
        let cli = Cli::parse_from(["agent-usage", "daily", "--no-cost", "--color", "never"]);
        let now = "2026-11-05T00:40:33Z".parse().unwrap();
        let next_reset: chrono::DateTime<chrono::Utc> = "2026-11-05T15:40:33Z".parse().unwrap();
        let mut scan = crate::model::Scan::default();
        for (source, age, used) in [("windows", 14, 92), ("wsl", 1, 93)] {
            scan.quota_snapshots.extend(crate::quota::QuotaSnapshot::parse(
                now - chrono::Duration::hours(age),
                &serde_json::json!({"primary":{"used_percent":used,"window_minutes":10080,"resets_at":next_reset.timestamp()}}),
                source,
            ));
        }
        let report =
            crate::report::build(&cli, chrono_tz::Asia::Shanghai, &scan, None, now).unwrap();
        (cli, report)
    }

    #[test]
    fn claude_selection_hides_quota_and_reset_in_normal_and_help_views() {
        let (cli, codex_report) = quota_report();
        let mut scan = crate::model::Scan::default();
        for agent in [
            crate::model::AgentKind::Codex,
            crate::model::AgentKind::ClaudeCode,
        ] {
            scan.sources.push(crate::model::SourceStatus {
                name: agent.to_string(),
                agent,
                root: "/unused".into(),
                readable: true,
                last_success: None,
                error: None,
                capability: crate::model::QuotaCapability::Unsupported,
                coverage: Default::default(),
                pricing_provider: None,
            });
        }
        let mut report =
            crate::report::build(&cli, chrono_tz::UTC, &scan, None, codex_report.generated_at)
                .unwrap();
        report.quotas = codex_report.quotas;
        assert!(!report.quotas.is_empty());
        assert!(quota_lines(&cli, &report, " / ").is_empty());
        let output = linear(&cli, &report, 120, false).join("\n");
        assert!(output.contains("Last 7 days"));
        assert!(
            !output.to_lowercase().contains("reset") && !output.contains("Quota"),
            "{output}"
        );
        let help = view(&cli, &report, &Style::new(&cli, false), 120, true)
            .body
            .join("\n");
        assert!(
            !help.to_lowercase().contains("reset") && !help.contains("Quota"),
            "{help}"
        );
    }

    #[test]
    fn quota_display_keeps_reset_without_source_diagnostics() {
        let (cli, report) = quota_report();
        let lines = quota_lines(&cli, &report, " · ");
        assert_eq!(lines, vec![(false, "Quota 7d · reset ~11-05 23:40".into())]);
        let rendered = linear(&cli, &report, 100, false).join("\n");
        assert!(rendered.contains("Since reset ~10-29 23:40 · Asia/Shanghai"));
        assert!(!rendered.contains("23:40:33"));
        assert!(!rendered.contains("stale") && !rendered.contains("forecast n/a"));
        assert!(view(&cli, &report, &Style::new(&cli, false), 120, true)
            .body
            .join("\n")
            .contains("2026-11-05T23:40:33+08:00"));
        // Matching display times are not matching deadlines in a different year.
        let mut old = report.clone();
        old.quotas[0].next_reset -= chrono::Duration::days(365);
        assert_eq!(quota_lines(&cli, &old, " · ").len(), 2);
    }

    #[test]
    fn quota_forecasts_and_different_windows_keep_their_identity() {
        let (cli, mut report) = quota_report();
        report.quotas[0].status = "current".into();
        report.quotas[0].remaining_tokens = Some(6600);
        report.quotas[0].exhausts_at = Some(report.generated_at + chrono::Duration::minutes(30));
        report.quotas[0].forecast_status = "estimated".into();
        let line = &quota_lines(&cli, &report, " · ")[0].1;
        assert!(line.contains("windows 92% ~6,600 tok left (exhaust ~11-05 09:10)"));
        assert!(!line.contains("wsl") && !line.contains("stale"));
        report.quotas[1].window_minutes = 300;
        assert_eq!(quota_lines(&cli, &report, " / ").len(), 2);
        assert!(quota_lines(&cli, &report, " / ")
            .iter()
            .all(|(_, line)| line.is_ascii()));
        report.quotas[1].window_minutes = 10080;
        report.quotas[1].source = "windows".into();
        report.quotas[1].bucket = "secondary".into();
        let line = &quota_lines(&cli, &report, " · ")[0].1;
        assert!(line.contains("windows/primary 92%"));
        assert!(!line.contains("secondary"));
        report.quotas[0].status = "expired".into();
        report.quotas[1].status = "expired".into();
        for quota in &mut report.quotas {
            quota.next_reset = report.generated_at - chrono::Duration::minutes(1);
        }
        assert_eq!(
            quota_lines(&cli, &report, " · ")[0].1,
            "Quota 7d · reset unavailable"
        );
    }
    #[test]
    fn tiny_price_is_visible() {
        assert_eq!(money(Some(Decimal::new(1, 8))), "<$0.0001");
    }
    #[test]
    fn wraps_by_display_cells_and_sanitizes_controls() {
        assert_eq!(wrap("模型ab", 4), vec!["模型", "ab"]);
        assert!(!wrap("evil\x1btext", 80).join("").contains('\x1b'));
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::{
        model::Scan,
        report::{self, Row},
    };
    use clap::Parser;
    #[test]
    fn session_selection_ranges_keep_full_ids_through_wrapping_and_help() {
        let cli = Cli::parse_from(["agent-usage", "sessions", "--all", "--color", "never"]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        for id in [
            "session-alpha-with-a-long-stable-identifier",
            "session-beta",
        ] {
            report.rows.push(Row {
                dimensions: [
                    ("session".into(), id.into()),
                    ("model".into(), "synthetic-model".into()),
                ]
                .into(),
                counts: Counts {
                    input: 10,
                    output: 5,
                    total: 15,
                    modes: ["regular".into()].into(),
                    ..Default::default()
                },
            });
        }
        report.rows[0]
            .dimensions
            .insert("mode".into(), "fast".into());
        for width in [40, 80, 140] {
            let view = view(&cli, &report, &Style::new(&cli, false), width, false);
            assert_eq!(view.session_rows.len(), 2);
            assert_eq!(
                view.session_rows[0].model.as_deref(),
                Some("synthetic-model")
            );
            assert_eq!(view.session_rows[0].mode.as_deref(), Some("fast"));
            assert_eq!(view.session_rows[1].mode.as_deref(), Some("regular"));
            assert_eq!(
                view.session_rows[0].session,
                "session-alpha-with-a-long-stable-identifier"
            );
            assert_eq!(view.session_rows[0].end, view.session_rows[1].start);
            assert_eq!(view.session_rows[1].end, view.body.len());
            assert!(view.session_rows.iter().all(|row| row.start < row.end));
        }
        assert!(view(&cli, &report, &Style::new(&cli, false), 80, true)
            .session_rows
            .is_empty());
        for row in &mut report.rows {
            row.dimensions.remove("session");
        }
        assert!(view(&cli, &report, &Style::new(&cli, false), 80, false)
            .session_rows
            .is_empty());
    }

    #[test]
    fn monitor_line_uses_rolling_deltas_instead_of_since_ready_totals() {
        let cli = Cli::parse_from(["agent-usage", "monitor", "--color", "never"]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        report.monitor = Some(report::MonitorInfo {
            sequence: 1,
            final_snapshot: false,
            since_ready_tokens: 1543980,
            since_ready_cost_usd: Some(Decimal::new(4369, 4)),
            window_tokens: 100,
            window_cost_usd: Some(Decimal::new(1, 2)),
            window_seconds: 2.0,
            sample_seconds: 2.0,
            window_cost_partial: false,
            output_tokens_per_second: 270.0,
            output_speed_history: Vec::new(),
            output_speed_window_seconds: 30,
            average_session_output_tokens_per_second: 33.0,
            session_output_tokens_per_second: Default::default(),
            session_speed_window_seconds: 60,
            agent_activity: Vec::new(),
            scan_millis: 0,
        });
        let output = linear(&cli, &report, 140, false).join("\n");
        assert!(
            output.contains("+100 tokens / +0.0100 USD / 270 out tok/s (33.0 tok/s)"),
            "{output}"
        );
        assert!(!output.contains("1543980") && !output.contains("+0.4369"));
    }

    #[test]
    fn monitor_range_shows_local_midnight_and_snapshot_time_including_rollover() {
        for (zone, now, date, range) in [
            (
                "Asia/Shanghai",
                "2026-11-03T18:34:56Z",
                "2026-11-04",
                "Today 00:00:00–02:34:56",
            ),
            (
                "Asia/Shanghai",
                "2026-11-04T15:59:59Z",
                "2026-11-04",
                "Today 00:00:00–23:59:59",
            ),
            (
                "Asia/Shanghai",
                "2026-11-04T16:00:01Z",
                "2026-11-05",
                "Today 00:00:00–00:00:01",
            ),
            (
                "America/New_York",
                "2026-11-01T06:30:00Z",
                "2026-11-01",
                "Today 00:00:00–01:30:00",
            ),
        ] {
            let cli = Cli::parse_from([
                "agent-usage",
                "monitor",
                "--timezone",
                zone,
                "--color",
                "never",
            ]);
            let report = report::build(
                &cli,
                zone.parse().unwrap(),
                &Scan::default(),
                None,
                now.parse().unwrap(),
            )
            .unwrap();
            let output = linear(&cli, &report, 140, false).join("\n");
            assert!(
                output.contains(range) && output.contains(date) && output.contains(zone),
                "{output}"
            );
            assert!(output.contains("n/a est."));
        }
    }

    #[test]
    fn selected_model_and_mode_survive_date_subtotal_and_mixed_mode_rows() {
        let cli = Cli::parse_from([
            "agent-usage",
            "sessions",
            "--all",
            "--group-by",
            "day,session,model",
            "--color",
            "never",
        ]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        report.rows.push(Row {
            dimensions: [
                ("day".into(), "2026-11-04".into()),
                ("session".into(), "full-stable-session-id".into()),
                ("model".into(), "gpt-5.6-sol".into()),
            ]
            .into(),
            counts: Counts {
                total: 30,
                modes: ["fast".into(), "regular".into()].into(),
                ..Default::default()
            },
        });
        report.subtotals.push(Row {
            dimensions: [("day".into(), "2026-11-04".into())].into(),
            counts: Counts {
                total: 30,
                ..Default::default()
            },
        });
        for width in [40, 80, 140] {
            let view = view(&cli, &report, &Style::new(&cli, false), width, false);
            let selected = &view.session_rows[0];
            assert_eq!(selected.session, "full-stable-session-id");
            assert_eq!(selected.model.as_deref(), Some("gpt-5.6-sol"));
            assert_eq!(
                selected.mode, None,
                "a mixed row must not invent a target mode"
            );
            assert_eq!(
                selected.end,
                view.body.len(),
                "trailing subtotal remains reachable"
            );
            assert!(view.body[selected.start..selected.end]
                .iter()
                .any(|line| line.contains("Subtotal")));
        }
    }

    #[test]
    fn monitor_marks_data_gaps_and_does_not_present_unknown_cost_as_free() {
        for readable in [true, false] {
            let cli = Cli::parse_from(["agent-usage", "monitor", "--color", "never"]);
            let mut scan = Scan::default();
            scan.sources.push(crate::model::SourceStatus {
                name: "local".into(),
                agent: crate::model::AgentKind::Codex,
                root: "/unused".into(),
                readable,
                last_success: None,
                error: None,
                capability: crate::model::QuotaCapability::RecordedSnapshots,
                coverage: Default::default(),
                pricing_provider: None,
            });
            scan.coverage.uncertain = usize::from(readable);
            let mut report =
                report::build(&cli, chrono_tz::UTC, &scan, None, chrono::Utc::now()).unwrap();
            report.totals.cost_usd = Some(Decimal::ZERO);
            let mut info = crate::monitor::Tracker::new(&scan, &report, true).info(0);
            info.window_cost_usd = None;
            info.window_cost_partial = true;
            report.monitor = Some(info);
            let output = linear(&cli, &report, 140, false).join("\n");
            assert!(
                output.contains("partial data")
                    && output.contains("n/a est.")
                    && output.contains("n/a USD"),
                "{output}"
            );
            assert!(!output.contains("+0.0000 USD") && !output.contains("$0.0000"));
            assert!(output.contains("Usage data incomplete."));
        }
    }

    #[test]
    fn monitor_partial_cost_and_filtered_scope_stay_compact_and_wrap() {
        let cli = Cli::parse_from([
            "agent-usage",
            "monitor",
            "--session",
            "s",
            "--project",
            "project",
            "--mode",
            "fast",
            "--color",
            "never",
        ]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        let mut info = crate::monitor::Tracker::new(&Scan::default(), &report, true).info(0);
        info.window_cost_usd = Some(Decimal::new(2, 2));
        info.window_cost_partial = true;
        info.average_session_output_tokens_per_second = 33.0;
        report.monitor = Some(info);
        let output = linear(&cli, &report, 140, false).join("\n");
        assert!(
            output.contains("LIVE FILTERED")
                && output.contains("Interval (2s):")
                && output.contains("+0 tokens / +0.0200 USD*")
                && output.contains("(33.0 tok/s)"),
            "{output}"
        );
        let help = view(&cli, &report, &Style::new(&cli, false), 140, true)
            .body
            .join("\n");
        assert!(
            help.contains("Filters:")
                && help.contains("project")
                && help.contains("unpriced usage")
        );
        for width in [24, 40, 80, 120] {
            let v = view(&cli, &report, &Style::new(&cli, false), width, false);
            assert!(v
                .header
                .iter()
                .chain(&v.body)
                .chain(&v.footer)
                .all(|line| line.width() <= width));
        }
    }

    #[test]
    fn no_cost_monitor_keeps_range_without_cost_fields_and_uses_ascii_separator() {
        let cli = Cli::parse_from([
            "agent-usage",
            "monitor",
            "--no-cost",
            "--ascii",
            "--color",
            "never",
        ]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            "2026-11-04T12:34:56Z".parse().unwrap(),
        )
        .unwrap();
        report.monitor =
            Some(crate::monitor::Tracker::new(&Scan::default(), &report, false).info(0));
        let output = linear(&cli, &report, 140, false).join("\n");
        assert!(
            output.contains("Today 00:00:00-12:34:56") && output.contains("Interval (2s):"),
            "{output}"
        );
        assert!(!output.contains("USD") && !output.contains("est."));
    }

    #[test]
    fn diagnostic_counts_are_only_in_on_demand_help() {
        let cli = Cli::parse_from(["agent-usage", "--color", "never"]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        report.coverage.uncertain = 6;
        report.totals.unpriced_tokens = 11_200_000;
        report.totals.assumed_events = 458;
        let style = Style::new(&cli, false);
        let normal = view(&cli, &report, &style, 120, false);
        let output = normal
            .header
            .iter()
            .chain(&normal.body)
            .chain(&normal.footer)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        for label in ["Unpriced:", "Data warnings:", "Pricing assumptions:"] {
            assert!(!output.contains(label));
        }
        assert!(normal.footer.last().unwrap().starts_with("Total "));
        let help = view(&cli, &report, &style, 120, true).body.join("\n");
        assert!(help.contains("Unpriced: 11.2M tokens"));
        assert!(help.contains("Data warnings: 6 / Pending: 0"));
        assert!(help.contains("Pricing assumptions: 458"));
        assert_eq!(report.coverage.uncertain, 6);
    }
    #[test]
    fn wide_stacked_view_keeps_detail_and_sanitizes_terminal_controls() {
        let cli = Cli::parse_from(["agent-usage", "--wide", "--no-cost", "--color", "never"]);
        let mut report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        report.rows.push(Row {
            dimensions: std::collections::BTreeMap::from([(
                "model".into(),
                "model\x1b[2J\n-name".into(),
            )]),
            counts: Counts {
                input: 100,
                cached: 50,
                output: 20,
                reasoning: 10,
                cache_write: 0,
                total: 120,
                events: 1,
                sessions: 1,
                ..Default::default()
            },
        });
        for width in [20, 40, 80, 140] {
            let v = view(&cli, &report, &Style::new(&cli, false), width, false);
            let body = v
                .header
                .iter()
                .chain(v.body.iter())
                .cloned()
                .collect::<String>();
            assert!(!body.contains('\x1b'));
            assert!(body.contains("Input"));
            assert!(body.contains("Reason"));
            assert!(body.contains("Sessions"));
            assert!(v
                .header
                .iter()
                .chain(v.body.iter())
                .chain(v.footer.iter())
                .all(|s| s.width() <= width));
        }
    }
}

#[cfg(test)]
mod agent_layout_tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn monitor_groups_models_and_indents_independent_activity() {
        let cli = Cli::parse_from(["agent-usage", "monitor", "--no-cost", "--color", "never"]);
        let mut report = crate::report::build(
            &cli,
            chrono_tz::UTC,
            &crate::model::Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        for (agent, model, tokens) in [
            ("claude-code", "claude-example", 860),
            ("codex", "gpt-example", 1240),
        ] {
            report.rows.push(crate::report::Row {
                dimensions: [
                    ("agent".into(), agent.into()),
                    ("provider".into(), "unknown".into()),
                    ("model".into(), model.into()),
                ]
                .into(),
                counts: Counts {
                    total: tokens,
                    ..Default::default()
                },
            });
        }
        let mut info =
            crate::monitor::Tracker::new(&crate::model::Scan::default(), &report, false).info(0);
        for (agent, tokens) in [
            (crate::model::AgentKind::Codex, 1240),
            (crate::model::AgentKind::ClaudeCode, 860),
        ] {
            info.agent_activity.push(crate::report::AgentActivity {
                agent,
                window_tokens: tokens,
                since_ready_tokens: 0,
                window_cost_usd: None,
                cost_partial: false,
                average_session_output_tokens_per_second: 42.0,
                output_tokens_per_second: 85.0,
                output_speed_history: vec![],
                output_speed_window_seconds: 30,
            });
        }
        report.totals.total = 2100;
        report.monitor = Some(info);
        for width in [60, 100, 160] {
            let output = linear(&cli, &report, width, false).join("\n");
            assert!(output.contains("Interval (2s):"), "{output}");
            assert!(output.contains("  Codex   +1240 tokens"), "{output}");
            assert!(output.contains("  Claude   +860 tokens"), "{output}");
            assert!(!output.contains("unknown /"));
            assert!(!output.contains(" USD"));
            assert!(output.find("gpt-example").unwrap() < output.find("claude-example").unwrap());
        }
    }
}

#[cfg(test)]
mod report_agent_sections_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn reports_nest_agents_inside_dates_without_provider_prefixes() {
        for command in ["daily", "monthly", "models", "sessions"] {
            let cli = Cli::parse_from([
                "agent-usage",
                command,
                "--all",
                "--no-cost",
                "--color",
                "never",
            ]);
            let mut report = crate::report::build(
                &cli,
                chrono_tz::UTC,
                &crate::model::Scan::default(),
                None,
                chrono::Utc::now(),
            )
            .unwrap();
            let period = match command {
                "daily" => Some(("day", "2026-11-10")),
                "monthly" => Some(("month", "2026-11")),
                _ => None,
            };
            for (agent, model) in [("claude-code", "claude-example"), ("codex", "gpt-example")] {
                let mut dimensions = [
                    ("agent".into(), agent.into()),
                    ("provider".into(), "unknown".into()),
                    ("model".into(), model.into()),
                ]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>();
                if let Some((key, date)) = period {
                    dimensions.insert(key.into(), date.into());
                }
                report.rows.push(crate::report::Row {
                    dimensions,
                    counts: Counts {
                        total: 10,
                        ..Default::default()
                    },
                });
            }
            if let Some((key, date)) = period {
                report.subtotals.push(crate::report::Row {
                    dimensions: [(key.into(), date.into())].into(),
                    counts: Counts {
                        total: 20,
                        ..Default::default()
                    },
                });
            }
            let output = linear(&cli, &report, 120, false).join("\n");
            assert!(
                output.lines().any(|line| line.trim() == "Codex")
                    && output.lines().any(|line| line.trim() == "Claude"),
                "{output}"
            );
            assert!(
                !output.contains("unknown") && !output.contains("codex /"),
                "{output}"
            );
            assert!(output.find("gpt-example").unwrap() < output.find("claude-example").unwrap());
            if period.is_some() {
                assert_eq!(output.matches("Subtotal").count(), 1, "{output}");
                let body = view(&cli, &report, &Style::new(&cli, false), 120, false).body;
                assert!(body.iter().all(|line| !line.trim().is_empty()), "{output}");
                assert!(output.contains("\n  Codex\n    gpt-example"), "{output}");
            }
        }
    }
}
