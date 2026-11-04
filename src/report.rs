use crate::{
    cli::{Cli, Command, Group, Sort},
    model::{Coverage, Scan, SourceStatus, UsageEvent},
    pricing::Pricer,
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
}
impl Period {
    pub fn make(cli: &Cli, tz: Tz, now: DateTime<Utc>) -> Result<Self> {
        if cli.has_monitor_range() {
            bail!("monitor summarizes today only; use daily for historical ranges");
        }
        let today = now.with_timezone(&tz).date_naive();
        let (start, end) = if cli.monitor() || cli.today {
            (Some(today), Some(today))
        } else if cli.all {
            (None, None)
        } else if cli.since.is_some() || cli.until.is_some() {
            (cli.since, cli.until)
        } else if let Some(days) = cli.last {
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
}
impl Counts {
    fn add(&mut self, event: &UsageEvent, pricer: Option<&Pricer>) -> Result<()> {
        let u = &event.usage;
        self.modes.insert(event.mode().to_string());
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
        if let Some(p) = pricer {
            let estimate = p.estimate(event);
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
            self.assumptions.extend(estimate.assumptions);
            if let Some(reason) = estimate.reason {
                self.pricing_reasons.insert(reason);
            }
            if let Some(id) = estimate.rate_id {
                self.rate_ids.insert(id);
            }
        }
        self.pricing_status = if pricer.is_none() {
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
        let label = ["source", "session", "project", "model"]
            .into_iter()
            .filter_map(|key| self.dimensions.get(key).cloned())
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
            "day", "month", "source", "session", "project", "model", "mode",
        ]
        .into_iter()
        .filter_map(|k| self.dimensions.get(k).cloned())
        .collect::<Vec<_>>()
        .join(" / ")
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub sequence: u64,
    pub final_snapshot: bool,
    pub since_ready_tokens: i128,
    pub since_ready_cost_usd: Option<Decimal>,
    pub output_tokens_per_second: f64,
    pub average_session_output_tokens_per_second: f64,
    pub session_output_tokens_per_second: BTreeMap<String, f64>,
    pub session_speed_window_seconds: u64,
    pub scan_millis: u128,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub kind: String,
    pub period: Period,
    pub rows: Vec<Row>,
    pub totals: Counts,
    pub sources: Vec<SourceStatus>,
    pub coverage: Coverage,
    pub pricing: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorInfo>,
}
pub fn matches(cli: &Cli, period: &Period, event: &UsageEvent) -> bool {
    period.includes(event.timestamp)
        && (cli.models.is_empty() || cli.models.contains(&event.model))
        && cli.mode.is_none_or(|mode| mode.as_str() == event.mode())
        && (cli.sessions.is_empty() || cli.sessions.contains(&event.session))
        && match &cli.command {
            Some(Command::Session { id }) => event.session == *id,
            _ => true,
        }
        && (cli.projects.is_empty()
            || cli.projects.iter().any(|p| {
                event.project.as_ref().is_some_and(|path| {
                    let normalized = path.replace('\\', "/");
                    normalized == p.replace('\\', "/")
                        || normalized.trim_end_matches('/').rsplit('/').next() == Some(p.as_str())
                })
            }))
}
pub fn build(
    cli: &Cli,
    tz: Tz,
    scan: &Scan,
    pricer: Option<&Pricer>,
    now: DateTime<Utc>,
) -> Result<Report> {
    let period = Period::make(cli, tz, now)?;
    let groups = cli.groups();
    let mut grouped: BTreeMap<Vec<String>, (Row, BTreeSet<String>)> = BTreeMap::new();
    let mut total = Counts::default();
    let mut sessions = BTreeSet::new();
    for event in scan.events.iter().filter(|e| matches(cli, &period, e)) {
        let dimensions: BTreeMap<String, String> = groups
            .iter()
            .map(|g| match g {
                Group::Model => ("model", event.model.clone()),
                Group::Mode => ("mode", event.mode().to_string()),
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
                Group::Session => ("session", event.session.clone()),
                Group::Project => ("project", event.project.clone().unwrap_or("unknown".into())),
            })
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let key = dimensions.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        let (row, group_sessions) = grouped.entry(key).or_insert_with(|| {
            (
                Row {
                    dimensions,
                    counts: Counts::default(),
                },
                BTreeSet::new(),
            )
        });
        row.counts.add(event, pricer)?;
        group_sessions.insert(event.session.clone());
        total.add(event, pricer)?;
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
    Ok(Report {
        schema_version: 1,
        generated_at: now,
        kind: cli.kind().into(),
        period,
        rows,
        totals: total,
        sources: scan.sources.clone(),
        coverage: scan.coverage.clone(),
        pricing: pricer
            .map(Pricer::metadata)
            .unwrap_or(serde_json::json!({"enabled":false})),
        monitor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn timezone_dates_and_end_bounds() {
        let cli = Cli::parse_from([
            "codex-usage",
            "--since",
            "2026-11-01",
            "--until",
            "2026-11-01",
        ]);
        let p = Period::make(&cli, chrono_tz::Asia::Shanghai, Utc::now()).unwrap();
        assert!(p.includes("2026-10-31T16:00:00Z".parse().unwrap()));
        assert!(!p.includes("2026-11-01T16:00:00Z".parse().unwrap()));
    }
    #[test]
    fn daylight_savings_day_has_correct_length() {
        let cli = Cli::parse_from(["codex-usage", "--today"]);
        let p = Period::make(
            &cli,
            chrono_tz::America::New_York,
            "2026-11-01T17:00:00Z".parse().unwrap(),
        )
        .unwrap();
        assert_eq!((p.end.unwrap() - p.start.unwrap()).num_hours(), 25);
    }
    #[test]
    fn last_days_uses_selected_timezone_and_includes_today() {
        let cli = Cli::parse_from(["codex-usage", "daily", "--last", "30"]);
        let p = Period::make(
            &cli,
            chrono_tz::Asia::Shanghai,
            "2026-11-04T18:00:00Z".parse().unwrap(),
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
        let cli = Cli::parse_from(["codex-usage", "daily", "--last", "7"]);
        let p = Period::make(
            &cli,
            chrono_tz::America::New_York,
            "2026-11-05T17:00:00Z".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(p.label, "2026-10-30–2026-11-05");
        assert_eq!((p.end.unwrap() - p.start.unwrap()).num_hours(), 169);
    }
    #[test]
    fn daily_explicit_ranges_and_other_report_defaults_are_preserved() {
        let now = "2026-11-04T18:00:00Z".parse().unwrap();
        for (args, expected) in [
            (vec!["codex-usage", "daily", "--today"], "2026-11-05"),
            (vec!["codex-usage", "daily", "--all"], "All time"),
            (
                vec![
                    "codex-usage",
                    "daily",
                    "--since",
                    "2026-11-01",
                    "--until",
                    "2026-11-03",
                ],
                "2026-11-01–2026-11-03",
            ),
            (vec!["codex-usage", "models"], "2026-11-01–2026-11-05"),
            (vec!["codex-usage", "daily"], "2026-11-01–2026-11-05"),
            (
                vec!["codex-usage", "daily", "--last", "7"],
                "2026-10-30–2026-11-05",
            ),
            (vec!["codex-usage", "daily", "--last", "1"], "2026-11-05"),
        ] {
            let cli = Cli::parse_from(args);
            assert_eq!(
                Period::make(&cli, chrono_tz::Asia::Shanghai, now)
                    .unwrap()
                    .label,
                expected
            );
        }
    }
    #[test]
    fn monitor_is_today_only_in_selected_timezone_and_rolls_at_midnight() {
        let cli = Cli::parse_from(["codex-usage", "monitor"]);
        let first = Period::make(
            &cli,
            chrono_tz::Asia::Shanghai,
            "2026-11-04T18:00:00Z".parse().unwrap(),
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
        )
        .unwrap();
        assert_eq!(next.label, "2026-11-06");
        assert_eq!(first.end, next.start);
        for args in [
            vec!["codex-usage", "monitor", "--all"],
            vec!["codex-usage", "monitor", "--since", "2026-11-01"],
            vec!["codex-usage", "monitor", "--until", "2026-11-01"],
            vec!["codex-usage", "monitor", "--last", "7"],
        ] {
            assert!(
                Period::make(&Cli::parse_from(args), chrono_tz::UTC, chrono::Utc::now()).is_err()
            );
        }
    }
}
