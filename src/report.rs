use crate::{
    cli::{Cli, Command, Group, Sort},
    model::{AgentKind, Coverage, Scan, SessionKey, SourceStatus, UsageEvent},
    pricing::{Estimate, Pricer},
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize)]
pub struct Period {
    pub label: String,
    pub timezone: String,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub basis: String,
}
impl Period {
    pub fn make(
        cli: &Cli,
        tz: Tz,
        now: DateTime<Utc>,
        agents: &BTreeSet<AgentKind>,
    ) -> Result<Self> {
        if cli.has_monitor_range() {
            bail!("monitor summarizes today only; use daily for historical ranges");
        }
        let today = now.with_timezone(&tz).date_naive();
        let default_daily = cli.default_daily_period();
        let (start, end) = if cli.monitor() || cli.today {
            (Some(today), Some(today))
        } else if cli.all {
            (None, None)
        } else if cli.since.is_some() || cli.until.is_some() {
            (cli.since, cli.until)
        } else if let Some(days) = cli.last.or_else(|| default_daily.then_some(7)) {
            (
                Some(
                    today
                        .checked_sub_days(Days::new(u64::from(
                            days.checked_sub(1).context("--last must be at least 1")?,
                        )))
                        .context("date out of range")?,
                ),
                Some(today),
            )
        } else {
            (
                Some(today.with_day(1).context("invalid month")?),
                Some(today),
            )
        };
        if start.zip(end).is_some_and(|(a, b)| a > b) {
            bail!("--since must not be after --until");
        }
        let midnight = |date: NaiveDate| -> Result<DateTime<Utc>> {
            tz.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .map(|t| t.with_timezone(&Utc))
                .ok_or_else(|| {
                    anyhow!("midnight does not exist in {tz} on {date}; select an adjacent date")
                })
        };
        Ok(Self {
            basis: if default_daily {
                if agents.contains(&AgentKind::ClaudeCode) {
                    if agents.len() > 1 {
                        "last_7_days_multi_agent"
                    } else {
                        "last_7_days_no_quota_capability"
                    }
                } else {
                    "last_7_days_fallback"
                }
            } else {
                "calendar"
            }
            .into(),
            label: match (start, end) {
                (None, None) => "All time".into(),
                (Some(a), Some(b)) if a == b => a.to_string(),
                (a, b) => format!(
                    "{}–{}",
                    a.map(|d| d.to_string()).unwrap_or("Beginning".into()),
                    b.map(|d| d.to_string()).unwrap_or("Now".into())
                ),
            },
            timezone: tz.to_string(),
            start: start.map(midnight).transpose()?,
            end: end
                .map(|d| {
                    d.checked_add_days(Days::new(1))
                        .context("date out of range")
                        .and_then(midnight)
                })
                .transpose()?,
        })
    }
    /// Resolve the reporting period from already loaded quota metadata only.
    /// Discovery continues to use `make` and never expands to hunt for a reset.
    fn with_quota(
        cli: &Cli,
        tz: Tz,
        now: DateTime<Utc>,
        quotas: &[crate::quota::QuotaSummary],
        sources: &[SourceStatus],
        agents: &BTreeSet<AgentKind>,
    ) -> Result<Self> {
        let mut period = Self::make(cli, tz, now, agents)?;
        // Quota-relative defaults are a Codex capability only. A Claude source,
        // even one with no events, keeps the combined range at seven days.
        if !cli.default_daily_period() || agents.iter().any(|agent| *agent != AgentKind::Codex) {
            return Ok(period);
        }
        // Select the most recent reset per source, including short-term windows.
        // Never assume independent roots belong to the same account.
        let mut resets = BTreeMap::new();
        for q in quotas {
            if q.limit_id != "codex" || q.status == "expired" {
                continue;
            }
            let Some(reset) = q
                .last_reset
                .filter(|reset| *reset <= now && *reset >= now - chrono::Duration::days(7))
            else {
                continue;
            };
            let entry = resets.entry(q.source.as_str()).or_insert((reset, true));
            if reset > entry.0 {
                *entry = (reset, q.last_reset_basis == "observed");
            } else if reset == entry.0 {
                entry.1 &= q.last_reset_basis == "observed";
            }
        }
        if resets.is_empty()
            || sources
                .iter()
                .any(|source| !source.readable || !resets.contains_key(source.name.as_str()))
        {
            return Ok(period);
        }
        let earliest = resets.values().map(|(r, _)| *r).min().unwrap();
        let latest = resets.values().map(|(r, _)| *r).max().unwrap();
        if (latest - earliest).num_seconds() > 60 {
            period.basis = "last_7_days_conflicting_resets".into();
            return Ok(period);
        }
        // A small server timestamp jitter must not drop the first few seconds.
        period.start = Some(earliest);
        let observed = resets.values().all(|(_, observed)| *observed);
        period.basis = if observed {
            "last_reset_observed"
        } else {
            "last_reset_estimated"
        }
        .into();
        period.label = format!(
            "Since reset {}{}",
            if observed { "" } else { "~" },
            earliest.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S")
        );
        Ok(period)
    }
    pub fn includes(&self, timestamp: DateTime<Utc>) -> bool {
        self.start.is_none_or(|t| timestamp >= t) && self.end.is_none_or(|t| timestamp < t)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total: u64,
    pub cached_missing: usize,
    pub cache_write_missing: usize,
    pub reasoning_missing: usize,
    pub events: usize,
    pub sessions: usize,
    pub cost_usd: Option<Decimal>,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
    pub assumed_events: usize,
    pub pricing_status: String,
    pub assumptions: BTreeSet<String>,
    pub pricing_reasons: BTreeSet<String>,
    pub rate_ids: BTreeSet<String>,
    pub modes: BTreeSet<String>,
    pub agents: BTreeSet<String>,
    pub providers: BTreeSet<String>,
}
impl Counts {
    fn add(&mut self, event: &UsageEvent, estimate: Option<&Estimate>) -> Result<()> {
        let u = &event.usage;
        self.modes.insert(event.mode().to_string());
        self.agents.insert(event.id.agent.to_string());
        self.providers.insert(event.provider.clone());
        macro_rules! sum {
            ($field:ident, $v:expr) => {
                self.$field = self
                    .$field
                    .checked_add($v)
                    .context("aggregate token overflow")?;
            };
        }
        sum!(input, u.input);
        sum!(cached, u.cached.unwrap_or(0));
        sum!(cache_write, u.cache_write.unwrap_or(0));
        sum!(output, u.output);
        sum!(reasoning, u.reasoning.unwrap_or(0));
        sum!(total, u.total());
        self.cached_missing += usize::from(u.cached.is_none());
        self.cache_write_missing += usize::from(u.cache_write.is_none());
        self.reasoning_missing += usize::from(u.reasoning.is_none());
        self.events += 1;
        if let Some(estimate) = estimate {
            if let Some(cost) = estimate.cost {
                self.cost_usd = Some(
                    self.cost_usd
                        .unwrap_or_default()
                        .checked_add(cost)
                        .context("cost overflow")?,
                );
                sum!(priced_tokens, u.total());
            } else {
                sum!(unpriced_tokens, u.total());
            }
            if !estimate.assumptions.is_empty() {
                self.assumed_events += 1;
            }
            self.assumptions
                .extend(estimate.assumptions.iter().cloned());
            if let Some(reason) = &estimate.reason {
                self.pricing_reasons.insert(reason.clone());
            }
            if let Some(id) = &estimate.rate_id {
                self.rate_ids.insert(id.clone());
            }
        }
        self.pricing_status = if estimate.is_none() {
            "disabled"
        } else if self.priced_tokens == 0 && self.unpriced_tokens > 0 {
            "unpriced"
        } else if self.unpriced_tokens > 0 {
            "partial"
        } else if self.assumed_events > 0 {
            "assumed"
        } else {
            "priced"
        }
        .into();
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub dimensions: BTreeMap<String, String>,
    pub counts: Counts,
}
impl Row {
    pub fn label(&self) -> String {
        let label = ["source", "agent", "provider", "session", "project", "model"]
            .into_iter()
            .filter_map(|key| {
                self.dimensions.get(key).cloned().map(|value| {
                    if key == "session"
                        && value
                            .parse::<SessionKey>()
                            .is_ok_and(|key| key.execution_id.is_some())
                    {
                        format!("{value} (subagent)")
                    } else {
                        value
                    }
                })
            })
            .collect::<Vec<_>>()
            .join(" / ");
        if label.is_empty() {
            "All models".into()
        } else {
            label
        }
    }
    pub fn identity(&self) -> String {
        [
            "day", "month", "source", "agent", "provider", "session", "project", "model", "mode",
        ]
        .into_iter()
        .filter_map(|k| self.dimensions.get(k).cloned())
        .collect::<Vec<_>>()
        .join(" / ")
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct SessionSpeed {
    pub session: SessionKey,
    pub output_tokens_per_second: f64,
}
/// Per-agent throughput and rolling chart history. Agents are measured
/// independently; their token deltas are never added to each other's series.
#[derive(Debug, Clone, Serialize)]
pub struct AgentActivity {
    pub agent: AgentKind,
    pub window_cost_usd: Option<Decimal>,
    pub cost_partial: bool,
    pub average_session_output_tokens_per_second: f64,
    pub since_ready_tokens: i128,
    pub window_tokens: i128,
    pub output_tokens_per_second: f64,
    pub output_speed_history: Vec<Option<f64>>,
    pub output_speed_window_seconds: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub sequence: u64,
    pub final_snapshot: bool,
    pub since_ready_tokens: i128,
    pub since_ready_cost_usd: Option<Decimal>,
    pub window_tokens: i128,
    pub window_cost_usd: Option<Decimal>,
    pub window_cost_partial: bool,
    pub window_seconds: f64,
    pub sample_seconds: f64,
    pub output_tokens_per_second: f64,
    pub output_speed_history: Vec<Option<f64>>,
    pub output_speed_window_seconds: u64,
    pub average_session_output_tokens_per_second: f64,
    pub session_output_tokens_per_second: Vec<SessionSpeed>,
    pub session_speed_window_seconds: u64,
    pub agent_activity: Vec<AgentActivity>,
    pub scan_millis: u128,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub kind: String,
    pub period: Period,
    pub rows: Vec<Row>,
    /// Outer date-section totals, separate from data rows to avoid additive exports.
    pub subtotals: Vec<Row>,
    pub totals: Counts,
    pub sources: Vec<SourceStatus>,
    pub coverage: Coverage,
    pub pricing: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quotas: Vec<crate::quota::QuotaSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorInfo>,
}
/// Argument-level selector error (exit status 2), distinct from processing
/// failures such as unreadable sources.
#[derive(Debug)]
pub struct SelectorError(pub String);
impl std::fmt::Display for SelectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for SelectorError {}

/// Resolved session selectors. A qualified selector matches exactly; a bare
/// native ID resolves only when it identifies one SessionKey.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub sessions_requested: bool,
    pub sessions: BTreeSet<SessionKey>,
    pub command_requested: bool,
    pub command: Option<SessionKey>,
}
fn resolve_one(text: &str, candidates: &BTreeSet<SessionKey>) -> Result<Option<SessionKey>> {
    if text.contains(':') {
        let key = text.parse::<SessionKey>().map_err(|error| {
            SelectorError(format!("Invalid session selector {text:?}: {error}"))
        })?;
        return Ok(Some(key));
    }
    let matches: Vec<_> = candidates
        .iter()
        .filter(|key| key.native_id == text)
        .cloned()
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [key] => Ok(Some(key.clone())),
        _ => Err(SelectorError(format!(
            "Session {text:?} is ambiguous; use a qualified selector: {}",
            matches
                .iter()
                .map(SessionKey::selector)
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .into()),
    }
}
impl Selection {
    pub fn resolve(cli: &Cli, scan: &Scan) -> Result<Self> {
        let mut candidates: BTreeSet<SessionKey> = scan.sessions.keys().cloned().collect();
        candidates.extend(scan.events.iter().map(|event| event.session.clone()));
        let mut selection = Self::default();
        if !cli.sessions.is_empty() {
            selection.sessions_requested = true;
            for text in &cli.sessions {
                if let Some(key) = resolve_one(text, &candidates)? {
                    selection.sessions.insert(key);
                }
            }
        }
        if let Some(Command::Session { id }) = &cli.command {
            selection.command_requested = true;
            selection.command = resolve_one(id, &candidates)?;
        }
        Ok(selection)
    }
}
pub fn matches(cli: &Cli, period: &Period, event: &UsageEvent, selection: &Selection) -> bool {
    period.includes(event.timestamp)
        && (cli.models.is_empty() || cli.models.contains(&event.model))
        && cli.mode.is_none_or(|mode| mode.as_str() == event.mode())
        && (!selection.sessions_requested || selection.sessions.contains(&event.session))
        && (!selection.command_requested || selection.command.as_ref() == Some(&event.session))
        && (cli.projects.is_empty()
            || cli.projects.iter().any(|p| {
                event.project.as_ref().is_some_and(|path| {
                    let normalized = path.replace('\\', "/");
                    normalized == p.replace('\\', "/")
                        || normalized.trim_end_matches('/').rsplit('/').next() == Some(p.as_str())
                })
            }))
}
/// Resolve the pricing scenario for a deduplicated event. Conflicting configured
/// providers across origins make pricing unavailable but retain token usage.
fn scenario_estimate(
    pricer: &Pricer,
    event: &UsageEvent,
    providers: &BTreeMap<String, Option<String>>,
) -> Estimate {
    let origins: Vec<&String> = if event.origins.is_empty() {
        vec![&event.source]
    } else {
        event.origins.iter().collect()
    };
    let configured: BTreeSet<&String> = origins
        .iter()
        .filter_map(|origin| providers.get(*origin).and_then(Option::as_ref))
        .collect();
    if configured.len() > 1 {
        return Estimate {
            cost: None,
            status: "unpriced".into(),
            assumptions: Vec::new(),
            reason: Some("Conflicting pricing scenarios across duplicate origins".into()),
            rate_id: None,
        };
    }
    pricer.estimate_scenario(event, configured.iter().next().map(|value| value.as_str()))
}
pub fn build(
    cli: &Cli,
    tz: Tz,
    scan: &Scan,
    pricer: Option<&Pricer>,
    now: DateTime<Utc>,
) -> Result<Report> {
    build_cancellable(
        cli,
        tz,
        scan,
        pricer,
        now,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

pub fn build_cancellable(
    cli: &Cli,
    tz: Tz,
    scan: &Scan,
    pricer: Option<&Pricer>,
    now: DateTime<Utc>,
    stopped: &std::sync::atomic::AtomicBool,
) -> Result<Report> {
    crate::cancel::check(stopped)?;
    let agents: BTreeSet<AgentKind> = scan.sources.iter().map(|source| source.agent).collect();
    let source_quality: BTreeMap<String, bool> = scan
        .sources
        .iter()
        .map(|source| {
            (
                source.name.clone(),
                !source.readable || source.coverage.incomplete(),
            )
        })
        .collect();
    let quotas = crate::quota::summarize(&scan.quota_snapshots, &scan.events, now, &source_quality);
    let period = Period::with_quota(cli, tz, now, &quotas, &scan.sources, &agents)?;
    let selection = Selection::resolve(cli, scan)?;
    let providers: BTreeMap<String, Option<String>> = scan
        .sources
        .iter()
        .map(|source| (source.name.clone(), source.pricing_provider.clone()))
        .collect();
    let groups = cli.groups();
    let date_key = if groups.contains(&Group::Day) {
        Some("day")
    } else if groups.contains(&Group::Month) {
        Some("month")
    } else {
        None
    };
    let mut grouped: BTreeMap<Vec<String>, (Row, BTreeSet<SessionKey>)> = BTreeMap::new();
    let mut dated: BTreeMap<String, (Row, BTreeSet<SessionKey>)> = BTreeMap::new();
    let mut total = Counts::default();
    let mut sessions = BTreeSet::new();
    for event in &scan.events {
        crate::cancel::check(stopped)?;
        if !matches(cli, &period, event, &selection) {
            continue;
        }
        let dimensions: BTreeMap<String, String> = groups
            .iter()
            .map(|g| match g {
                Group::Model => ("model", event.model.clone()),
                Group::Mode => ("mode", event.mode().to_string()),
                Group::Agent => ("agent", event.id.agent.to_string()),
                Group::Provider => ("provider", event.provider.clone()),
                Group::Source => ("source", event.source.clone()),
                Group::Day => (
                    "day",
                    event
                        .timestamp
                        .with_timezone(&tz)
                        .format("%Y-%m-%d")
                        .to_string(),
                ),
                Group::Month => (
                    "month",
                    event
                        .timestamp
                        .with_timezone(&tz)
                        .format("%Y-%m")
                        .to_string(),
                ),
                Group::Session => ("session", event.session.selector()),
                Group::Project => ("project", event.project.clone().unwrap_or("unknown".into())),
            })
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let key = dimensions.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        let date = date_key.and_then(|key| dimensions.get(key).cloned());
        let (row, group_sessions) = grouped.entry(key).or_insert_with(|| {
            (
                Row {
                    dimensions,
                    counts: Counts::default(),
                },
                BTreeSet::new(),
            )
        });
        let estimate = pricer.map(|pricer| scenario_estimate(pricer, event, &providers));
        row.counts.add(event, estimate.as_ref())?;
        group_sessions.insert(event.session.clone());
        if let (Some(date_key), Some(date)) = (date_key, date) {
            let (subtotal, date_sessions) = dated.entry(date.clone()).or_insert_with(|| {
                (
                    Row {
                        dimensions: [(date_key.to_string(), date)].into(),
                        counts: Counts::default(),
                    },
                    BTreeSet::new(),
                )
            });
            subtotal.counts.add(event, estimate.as_ref())?;
            date_sessions.insert(event.session.clone());
        }
        total.add(event, estimate.as_ref())?;
        sessions.insert(event.session.clone());
    }
    total.sessions = sessions.len();
    if total.events == 0 {
        total.pricing_status = if pricer.is_some() {
            "priced"
        } else {
            "disabled"
        }
        .into();
        total.cost_usd = pricer.map(|_| Decimal::ZERO);
    }
    let mut rows: Vec<Row> = grouped
        .into_values()
        .map(|(mut r, s)| {
            r.counts.sessions = s.len();
            r
        })
        .collect();
    let subtotals = dated
        .into_values()
        .map(|(mut row, sessions)| {
            row.counts.sessions = sessions.len();
            row
        })
        .collect();
    let sort = cli.sort.unwrap_or(if cli.monitor() {
        Sort::Model
    } else {
        Sort::Total
    });
    rows.sort_by(|a, b| {
        let cmp = match sort {
            Sort::Model => a.identity().cmp(&b.identity()),
            Sort::Input => b.counts.input.cmp(&a.counts.input),
            Sort::Output => b.counts.output.cmp(&a.counts.output),
            Sort::Total => b.counts.total.cmp(&a.counts.total),
            Sort::Cost => b.counts.cost_usd.cmp(&a.counts.cost_usd),
        };
        // Time-based reports remain chronological; selected sort applies within each period.
        let time = a
            .dimensions
            .get("day")
            .or(a.dimensions.get("month"))
            .cmp(&b.dimensions.get("day").or(b.dimensions.get("month")));
        time.then(cmp).then_with(|| a.identity().cmp(&b.identity()))
    });
    crate::cancel::check(stopped)?;
    Ok(Report {
        schema_version: 2,
        generated_at: now,
        kind: cli.kind().into(),
        period,
        rows,
        subtotals,
        totals: total,
        sources: scan.sources.clone(),
        coverage: scan.coverage.clone(),
        pricing: pricer
            .map(Pricer::metadata)
            .unwrap_or(serde_json::json!({"enabled":false})),
        monitor: None,
        quotas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventKey, Usage};
    use clap::Parser;

    fn event(id: &str, session: &str, model: &str, tier: Option<&str>, time: &str) -> UsageEvent {
        UsageEvent {
            id: EventKey::new(AgentKind::Codex, id),
            timestamp: time.parse().unwrap(),
            session: SessionKey::main(AgentKind::Codex, session),
            model: model.into(),
            provider: "openai".into(),
            tier: tier.map(str::to_owned),
            mode: match tier {
                Some("priority" | "fast") => crate::model::UsageMode::Fast,
                Some(_) => crate::model::UsageMode::Regular,
                None => crate::model::UsageMode::Unknown,
            },
            project: Some("/work/test-project".into()),
            usage: Usage {
                input: 100,
                cached: Some(60),
                cache_write: Some(0),
                output: 20,
                reasoning: Some(10),
            },
            cache_writes: None,
            source: "fixture".into(),
            origins: BTreeSet::new(),
            context_tokens: None,
        }
    }
    fn agents() -> BTreeSet<AgentKind> {
        [AgentKind::Codex].into()
    }

    fn dated_report(args: &[&str], scan: &Scan, pricer: Option<&Pricer>, tz: Tz) -> Report {
        let cli = Cli::parse_from(std::iter::once("agent-usage").chain(args.iter().copied()));
        build(
            &cli,
            tz,
            scan,
            pricer,
            "2026-11-04T12:00:00Z".parse().unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn date_subtotals_count_sessions_once_across_model_and_mode_rows() {
        let scan = Scan {
            events: vec![
                event(
                    "a",
                    "shared",
                    "gpt-6.1-sol",
                    Some("standard"),
                    "2026-11-01T00:00:00Z",
                ),
                event(
                    "b",
                    "shared",
                    "gpt-5.6-sol",
                    Some("standard"),
                    "2026-11-01T01:00:00Z",
                ),
                event(
                    "c",
                    "other",
                    "gpt-6.1-sol",
                    Some("fast"),
                    "2026-11-01T02:00:00Z",
                ),
                event(
                    "d",
                    "shared",
                    "gpt-6.1-sol",
                    Some("standard"),
                    "2026-11-02T00:00:00Z",
                ),
            ],
            ..Default::default()
        };
        let pricer = Pricer::load_current(None).unwrap();
        let report = dated_report(
            &["daily", "--all", "--sort", "cost"],
            &scan,
            Some(&pricer),
            chrono_tz::UTC,
        );
        assert_eq!(report.rows.len(), 4);
        assert_eq!(report.subtotals.len(), 2);
        assert_eq!(
            report.subtotals[0].dimensions,
            [("day".into(), "2026-11-01".into())].into()
        );
        assert_eq!(report.subtotals[0].counts.total, 360);
        assert_eq!(report.subtotals[0].counts.sessions, 2);
        assert_eq!(report.subtotals[0].counts.events, 3);
        assert_eq!(
            report.subtotals[0].counts.cost_usd,
            Some(Decimal::new(1442, 6))
        );
        assert_eq!(report.subtotals[1].dimensions["day"], "2026-11-02");
        assert_eq!(report.subtotals[1].counts.total, 120);
        assert_eq!(report.subtotals[1].counts.sessions, 1);
        assert_eq!(report.totals.total, 480);
        assert_eq!(report.totals.sessions, 2);
        assert_eq!(report.totals.cost_usd, Some(Decimal::new(1728, 6)));
        assert_eq!(
            report.subtotals.iter().map(|r| r.counts.total).sum::<u64>(),
            report.totals.total
        );
    }

    #[test]
    fn date_subtotals_preserve_partial_assumed_unpriced_and_disabled_costs() {
        let scan = Scan {
            events: vec![
                event(
                    "a",
                    "shared",
                    "gpt-6.1-sol",
                    Some("standard"),
                    "2026-11-01T00:00:00Z",
                ),
                event(
                    "b",
                    "shared",
                    "unknown-model",
                    Some("standard"),
                    "2026-11-01T01:00:00Z",
                ),
                event("c", "other", "gpt-6.1-sol", None, "2026-11-01T02:00:00Z"),
                event(
                    "d",
                    "shared",
                    "unknown-model",
                    Some("standard"),
                    "2026-11-02T00:00:00Z",
                ),
            ],
            ..Default::default()
        };
        let pricer = Pricer::load_current(None).unwrap();
        let report = dated_report(&["daily", "--all"], &scan, Some(&pricer), chrono_tz::UTC);
        let mixed = &report.subtotals[0].counts;
        assert_eq!(mixed.total, 360);
        assert_eq!(mixed.priced_tokens, 240);
        assert_eq!(mixed.unpriced_tokens, 120);
        assert_eq!(mixed.assumed_events, 1);
        assert_eq!(mixed.pricing_status, "partial");
        assert_eq!(mixed.cost_usd, Some(Decimal::new(572, 6)));
        assert!(!mixed.assumptions.is_empty());
        let unknown = &report.subtotals[1].counts;
        assert_eq!(unknown.pricing_status, "unpriced");
        assert_eq!(unknown.cost_usd, None);
        assert_eq!(unknown.unpriced_tokens, 120);
        let disabled = dated_report(
            &["daily", "--all", "--no-cost"],
            &scan,
            None,
            chrono_tz::UTC,
        );
        assert!(disabled
            .subtotals
            .iter()
            .all(|r| r.counts.pricing_status == "disabled" && r.counts.cost_usd.is_none()));
    }

    #[test]
    fn date_subtotals_follow_timezone_filters_and_outer_day_or_month_dimension() {
        let scan = Scan {
            events: vec![
                event(
                    "a",
                    "shared",
                    "gpt-6.1-sol",
                    Some("standard"),
                    "2026-10-31T15:59:59Z",
                ),
                event(
                    "b",
                    "shared",
                    "gpt-6.1-sol",
                    Some("fast"),
                    "2026-10-31T16:00:00Z",
                ),
                event(
                    "c",
                    "other",
                    "gpt-5.6-sol",
                    Some("standard"),
                    "2026-11-01T16:00:00Z",
                ),
            ],
            ..Default::default()
        };
        let monthly = dated_report(
            &["monthly", "--all"],
            &scan,
            None,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(monthly.subtotals.len(), 2);
        assert_eq!(
            monthly.subtotals[0].dimensions,
            [("month".into(), "2026-10".into())].into()
        );
        assert_eq!(monthly.subtotals[0].counts.total, 120);
        assert_eq!(monthly.subtotals[1].dimensions["month"], "2026-11");
        assert_eq!(monthly.subtotals[1].counts.total, 240);
        let both = dated_report(
            &["models", "--all", "--group-by", "month,day,model"],
            &scan,
            None,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(both.subtotals.len(), 3);
        assert!(both
            .subtotals
            .iter()
            .all(|r| r.dimensions.len() == 1 && r.dimensions.contains_key("day")));
        let filtered = dated_report(
            &[
                "daily",
                "--since",
                "2026-11-01",
                "--until",
                "2026-11-01",
                "--model",
                "gpt-6.1-sol",
                "--mode",
                "fast",
                "--project",
                "test-project",
            ],
            &scan,
            None,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(filtered.subtotals.len(), 1);
        assert_eq!(filtered.subtotals[0].dimensions["day"], "2026-11-01");
        assert_eq!(filtered.subtotals[0].counts.total, 120);
        let ungrouped = dated_report(
            &["daily", "--all", "--group-by", "model,mode"],
            &scan,
            None,
            chrono_tz::UTC,
        );
        assert!(ungrouped.subtotals.is_empty());
        let empty = dated_report(&["daily", "--all"], &Scan::default(), None, chrono_tz::UTC);
        assert!(empty.subtotals.is_empty());
        assert_eq!(empty.totals.total, 0);
    }
    #[test]
    fn timezone_dates_and_end_bounds() {
        let cli = Cli::parse_from([
            "agent-usage",
            "--since",
            "2026-11-01",
            "--until",
            "2026-11-01",
        ]);
        let p = Period::make(&cli, chrono_tz::Asia::Shanghai, Utc::now(), &agents()).unwrap();
        assert!(p.includes("2026-10-31T16:00:00Z".parse().unwrap()));
        assert!(!p.includes("2026-11-01T16:00:00Z".parse().unwrap()));
    }
    #[test]
    fn daylight_savings_day_has_correct_length() {
        let cli = Cli::parse_from(["agent-usage", "--today"]);
        let p = Period::make(
            &cli,
            chrono_tz::America::New_York,
            "2026-11-01T17:00:00Z".parse().unwrap(),
            &agents(),
        )
        .unwrap();
        assert_eq!((p.end.unwrap() - p.start.unwrap()).num_hours(), 25);
    }
    #[test]
    fn last_days_uses_selected_timezone_and_includes_today() {
        let cli = Cli::parse_from(["agent-usage", "daily", "--last", "30"]);
        let p = Period::make(
            &cli,
            chrono_tz::Asia::Shanghai,
            "2026-11-04T18:00:00Z".parse().unwrap(),
            &agents(),
        )
        .unwrap();
        assert_eq!(p.label, "2026-10-07–2026-11-05");
        assert_eq!(
            p.start.unwrap(),
            "2026-10-06T16:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(
            p.end.unwrap(),
            "2026-11-05T16:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert!(p.includes("2026-10-06T16:00:00Z".parse().unwrap()));
        assert!(!p.includes("2026-10-06T15:59:59Z".parse().unwrap()));
        assert!(!p.includes(p.end.unwrap()));
    }
    #[test]
    fn last_days_counts_calendar_dates_across_daylight_saving() {
        let cli = Cli::parse_from(["agent-usage", "daily"]);
        let p = Period::make(
            &cli,
            chrono_tz::America::New_York,
            "2026-11-05T17:00:00Z".parse().unwrap(),
            &agents(),
        )
        .unwrap();
        assert_eq!(p.label, "2026-10-30–2026-11-05");
        assert_eq!((p.end.unwrap() - p.start.unwrap()).num_hours(), 169);
    }
    #[test]
    fn daily_explicit_ranges_and_other_report_defaults_are_preserved() {
        let now = "2026-11-04T18:00:00Z".parse().unwrap();
        for (args, expected) in [
            (vec!["agent-usage", "daily", "--today"], "2026-11-05"),
            (vec!["agent-usage", "daily", "--all"], "All time"),
            (
                vec![
                    "agent-usage",
                    "daily",
                    "--since",
                    "2026-11-01",
                    "--until",
                    "2026-11-03",
                ],
                "2026-11-01–2026-11-03",
            ),
            (vec!["agent-usage", "models"], "2026-11-01–2026-11-05"),
            (vec!["agent-usage"], "2026-10-30–2026-11-05"),
            (vec!["agent-usage", "daily"], "2026-10-30–2026-11-05"),
            (vec!["agent-usage", "monthly"], "2026-11-01–2026-11-05"),
            (vec!["agent-usage", "sessions"], "2026-11-01–2026-11-05"),
            (
                vec!["agent-usage", "session", "example"],
                "2026-11-01–2026-11-05",
            ),
            (
                vec!["agent-usage", "daily", "--last", "7"],
                "2026-10-30–2026-11-05",
            ),
            (vec!["agent-usage", "daily", "--last", "1"], "2026-11-05"),
        ] {
            let cli = Cli::parse_from(args);
            assert_eq!(
                Period::make(&cli, chrono_tz::Asia::Shanghai, now, &agents())
                    .unwrap()
                    .label,
                expected
            );
        }
    }
    #[test]
    fn monitor_is_today_only_in_selected_timezone_and_rolls_at_midnight() {
        let cli = Cli::parse_from(["agent-usage", "monitor"]);
        let first = Period::make(
            &cli,
            chrono_tz::Asia::Shanghai,
            "2026-11-04T18:00:00Z".parse().unwrap(),
            &agents(),
        )
        .unwrap();
        assert_eq!(first.label, "2026-11-05");
        assert!(first.includes("2026-11-04T16:00:00Z".parse().unwrap()));
        assert!(!first.includes("2026-11-04T15:59:59Z".parse().unwrap()));
        assert!(!first.includes("2026-11-05T16:00:00Z".parse().unwrap()));
        let next = Period::make(
            &cli,
            chrono_tz::Asia::Shanghai,
            "2026-11-05T16:00:00Z".parse().unwrap(),
            &agents(),
        )
        .unwrap();
        assert_eq!(next.label, "2026-11-06");
        assert_eq!(first.end, next.start);
        for args in [
            vec!["agent-usage", "monitor", "--all"],
            vec!["agent-usage", "monitor", "--since", "2026-11-01"],
            vec!["agent-usage", "monitor", "--until", "2026-11-01"],
            vec!["agent-usage", "monitor", "--last", "7"],
        ] {
            assert!(Period::make(
                &Cli::parse_from(args),
                chrono_tz::UTC,
                chrono::Utc::now(),
                &agents()
            )
            .is_err());
        }
    }

    fn status(name: &str, agent: AgentKind) -> SourceStatus {
        SourceStatus {
            name: name.into(),
            agent,
            root: format!("/{name}").into(),
            readable: true,
            last_success: Some(Utc::now()),
            error: None,
            capability: match agent {
                AgentKind::Codex => crate::model::QuotaCapability::RecordedSnapshots,
                AgentKind::ClaudeCode => crate::model::QuotaCapability::Unsupported,
            },
            coverage: Coverage::default(),
            pricing_provider: None,
        }
    }
    fn cross_agent_scan() -> Scan {
        let mut codex = event(
            "same",
            "shared",
            "gpt-6.1-sol",
            Some("standard"),
            "2026-11-01T00:00:00Z",
        );
        codex.id = EventKey::new(AgentKind::Codex, "message:same");
        codex.session = SessionKey::main(AgentKind::Codex, "shared");
        let mut claude = event(
            "same",
            "shared",
            "claude-test",
            None,
            "2026-11-01T00:00:00Z",
        );
        claude.id = EventKey::new(AgentKind::ClaudeCode, "message:same");
        claude.session = SessionKey::main(AgentKind::ClaudeCode, "shared");
        claude.provider = "unknown".into();
        claude.mode = crate::model::UsageMode::Regular;
        Scan {
            events: vec![codex, claude],
            sources: vec![
                status("codex-root", AgentKind::Codex),
                status("claude-root", AgentKind::ClaudeCode),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn equal_native_ids_across_agents_remain_distinct_in_rows_counts_and_csv() {
        let cli = Cli::parse_from([
            "agent-usage",
            "sessions",
            "--all",
            "--no-cost",
            "--color",
            "never",
        ]);
        let report = build(&cli, chrono_tz::UTC, &cross_agent_scan(), None, Utc::now()).unwrap();
        assert_eq!(report.schema_version, 2);
        assert_eq!(report.totals.events, 2);
        assert_eq!(report.totals.sessions, 2);
        assert_eq!(report.totals.agents.len(), 2);
        assert_eq!(report.rows.len(), 2);
        let sessions: BTreeSet<_> = report
            .rows
            .iter()
            .map(|row| row.dimensions["session"].clone())
            .collect();
        assert_eq!(
            sessions,
            ["codex:shared".into(), "claude-code:shared".into()].into()
        );
        let mut csv = Vec::new();
        crate::export::csv(&report, &mut csv).unwrap();
        let text = String::from_utf8(csv).unwrap();
        assert!(text.starts_with(
            "schema_version,agent,provider,day,month,source,session,project,model,mode,agents,providers,"
        ));
        assert!(text.lines().nth(1).unwrap().starts_with("2,"));
        assert!(text.contains("\"[\"\"codex\"\"]\""));
        assert!(text.contains("claude-code:shared"));
    }

    #[test]
    fn ambiguous_bare_session_ids_require_a_qualified_selector() {
        let scan = cross_agent_scan();
        let bare = Cli::parse_from(["agent-usage", "session", "shared", "--all"]);
        let error = Selection::resolve(&bare, &scan).unwrap_err().to_string();
        assert!(error.contains("ambiguous"));
        assert!(error.contains("codex:shared") && error.contains("claude-code:shared"));
        let qualified = Cli::parse_from(["agent-usage", "session", "claude-code:shared", "--all"]);
        let selection = Selection::resolve(&qualified, &scan).unwrap();
        assert_eq!(
            selection.command,
            Some(SessionKey::main(AgentKind::ClaudeCode, "shared"))
        );
        // A bare ID that is unique still resolves.
        let mut single = scan;
        single
            .events
            .retain(|event| event.id.agent == AgentKind::Codex);
        let bare = Cli::parse_from(["agent-usage", "session", "shared", "--all"]);
        assert_eq!(
            Selection::resolve(&bare, &single).unwrap().command,
            Some(SessionKey::main(AgentKind::Codex, "shared"))
        );
    }

    #[test]
    fn claude_sources_keep_the_combined_daily_default_at_seven_days() {
        let now = "2026-11-04T18:00:00Z".parse().unwrap();
        let reset = now - chrono::Duration::days(2);
        let quota = crate::quota::QuotaSummary {
            source: "codex-root".into(),
            limit_id: "codex".into(),
            bucket: "primary".into(),
            used_percent: 50.0,
            window_minutes: 300,
            observed_at: now,
            next_reset: now + chrono::Duration::hours(1),
            last_reset: Some(reset),
            last_reset_basis: "observed".into(),
            status: "current".into(),
            remaining_tokens: None,
            exhausts_at: None,
            forecast_status: "insufficient_history".into(),
            forecast_window_seconds: None,
            forecast_used_percent: None,
            forecast_tokens: None,
        };
        let sources = vec![
            status("codex-root", AgentKind::Codex),
            status("claude-root", AgentKind::ClaudeCode),
        ];
        let cli = Cli::parse_from(["agent-usage", "daily"]);
        let mixed = [AgentKind::Codex, AgentKind::ClaudeCode]
            .into_iter()
            .collect();
        let period = Period::with_quota(
            &cli,
            chrono_tz::Asia::Shanghai,
            now,
            std::slice::from_ref(&quota),
            &sources,
            &mixed,
        )
        .unwrap();
        assert_eq!(period.basis, "last_7_days_multi_agent");
        assert_eq!(period.label, "2026-10-30–2026-11-05");

        let claude_only = [AgentKind::ClaudeCode].into_iter().collect();
        let period = Period::with_quota(
            &cli,
            chrono_tz::Asia::Shanghai,
            now,
            std::slice::from_ref(&quota),
            &sources,
            &claude_only,
        )
        .unwrap();
        assert_eq!(period.basis, "last_7_days_no_quota_capability");

        let codex_only = [AgentKind::Codex].into_iter().collect();
        let period = Period::with_quota(
            &cli,
            chrono_tz::Asia::Shanghai,
            now,
            &[quota],
            &sources[..1],
            &codex_only,
        )
        .unwrap();
        assert_eq!(period.basis, "last_reset_observed");
        assert_eq!(period.start, Some(reset));
    }
}
