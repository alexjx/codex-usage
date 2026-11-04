use crate::{
    cli::{Cli, Color, Theme},
    report::{Counts, Report},
};
use rust_decimal::{Decimal, RoundingStrategy};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    pub body: Vec<String>,
    pub footer: Vec<String>,
    pub identities: Vec<(String, usize)>,
    pub sections: Vec<(String, usize)>,
}
pub fn view(cli: &Cli, report: &Report, style: &Style, width: usize, help: bool) -> View {
    let mut h = Vec::new();
    let mut b = Vec::new();
    let mut total_footer = Vec::new();
    let mut identities = Vec::new();
    let mut sections = Vec::new();
    let sep = if cli.ascii { " / " } else { " · " };
    text(
        &mut h,
        style,
        "accent",
        &format!(
            "Codex Usage / {}{}{sep}{}{sep}{}",
            report.kind,
            if cli.monitor() { " LIVE" } else { "" },
            report.period.label,
            report.period.timezone
        ),
        width,
    );
    // Style source status independently while keeping paths out of the primary view.
    let status = report
        .sources
        .iter()
        .map(|s| format!("{} {}", s.name, if s.readable { "ready" } else { "stale" }))
        .collect::<Vec<_>>()
        .join(sep);
    text(
        &mut h,
        style,
        if report.sources.iter().all(|s| s.readable) {
            "ok"
        } else {
            "warn"
        },
        &status,
        width,
    );
    let mut summary = format!("{} tokens", number(report.totals.total, cli.exact));
    if !cli.no_cost {
        summary.push_str(&format!(
            "{sep}{} est.{}",
            money(report.totals.cost_usd),
            if report.totals.unpriced_tokens > 0 && report.totals.priced_tokens > 0 {
                " subtotal"
            } else {
                ""
            }
        ));
    }
    text(&mut h, style, "cost", &summary, width);
    if let Some(m) = &report.monitor {
        let mut delta = format!("{:+} tokens", m.since_ready_tokens);
        if !cli.no_cost {
            if let Some(cost) = m.since_ready_cost_usd {
                delta.push_str(&format!(" / {:+.4} USD", cost));
            }
        }
        let speed = if m.output_tokens_per_second > 0.0 && m.output_tokens_per_second < 10.0 {
            format!("{:.1}", m.output_tokens_per_second)
        } else {
            format!("{:.0}", m.output_tokens_per_second)
        };
        delta.push_str(&format!(" / {speed} out tok/s"));
        delta.push_str(&format!(
            " ({:.1} tok/s)",
            m.average_session_output_tokens_per_second
        ));
        text(&mut h, style, "accent", &delta, width);
    }
    let labels = if help {
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
        .map(|r| r.label().width())
        .max()
        .unwrap_or(16)
        .max(labels[0].len());
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
    if help {
        text(&mut h, style, "meta", "Help", width);
        let mut details=vec!["Arrows/j/k Scroll | PgUp/PgDn Page | Home/End | q Quit | ? Back".into(),
            "Summary covers the full selection. Cached is part of input; reasoning is part of output. * marks partial coverage.".into(),
            "Speed: newly observed output tokens over the last 5s; delayed backlog and corrections are excluded.".into(),
            "Speed in parentheses: mean output rate per active session over 30s (shorter since startup). Idle sessions drop out. Logs arrive in batches; this is observed throughput, not streaming generation speed. Individual rates are available in JSONL.".into(),
            format!("Unpriced: {} tokens",number(report.totals.unpriced_tokens,cli.exact)),
            format!("Data warnings: {} / Pending: {}",report.coverage.malformed+report.coverage.conflicts+report.coverage.uncertain,report.coverage.pending),
            format!("Pricing assumptions: {}",report.totals.assumed_events)];
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
        let mut previous_date = None;
        for row in &report.rows {
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
                    text(&mut b, style, "accent", &label, width);
                    previous_date = Some(date);
                }
            }
            let id = row.label();
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
                b.push(String::new());
            } else {
                let vs = values(&row.counts, cli, detailed);
                let mut line = style.paint("bold", pad(&id, id_width, false));
                for (i, (value, w)) in vs.iter().zip(&widths).enumerate() {
                    line.push_str("  ");
                    let role = if i == 0 && mode_label(&row.counts) == "Fast" {
                        "accent"
                    } else if !cli.no_cost && i == vs.len() - 1 {
                        "cost"
                    } else {
                        "body"
                    };
                    line.push_str(&style.paint(role, pad(value, *w, true)));
                }
                b.push(line);
            }
        }
        if report.rows.is_empty() {
            text(&mut b, style, "meta", "No usage in this period.", width);
        }
        total_footer.push(String::new());
        let mut totals = values(&report.totals, cli, detailed);
        // A grand total spans all modes; leave that categorical cell empty.
        totals[0].clear();
        if stacked {
            text(&mut total_footer, style, "bold", "Total", width);
            let detail = if cli.wide {
                titles
                    .iter()
                    .zip(&totals)
                    .skip(1)
                    .map(|(label, value)| format!("{label} {value}"))
                    .collect::<Vec<_>>()
                    .join(" / ")
            } else {
                let mut detail = format!("{} tokens", number(report.totals.total, cli.exact));
                if !cli.no_cost {
                    detail.push_str(&format!(" / {}", totals.last().unwrap()));
                }
                detail
            };
            text(&mut total_footer, style, "bold", &detail, width);
        } else {
            let mut line = style.paint("bold", pad("Total", id_width, false));
            for (i, (value, w)) in totals.iter().zip(&widths).enumerate() {
                line.push_str("  ");
                line.push_str(&style.paint(
                    if !cli.no_cost && i == totals.len() - 1 {
                        "cost"
                    } else {
                        "bold"
                    },
                    pad(value, *w, true),
                ));
            }
            total_footer.push(line);
        }
    }
    View {
        header: h,
        body: b,
        footer: total_footer,
        identities,
        sections,
    }
}

pub fn linear(cli: &Cli, report: &Report, width: usize, tty: bool) -> Vec<String> {
    let v = view(cli, report, &Style::new(cli, tty), width, false);
    v.header.into_iter().chain(v.body).chain(v.footer).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn diagnostic_counts_are_only_in_on_demand_help() {
        let cli = Cli::parse_from(["codex-usage", "--color", "never"]);
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
        let cli = Cli::parse_from(["codex-usage", "--wide", "--no-cost", "--color", "never"]);
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
