//! Compact rolling output-speed history for the full-screen monitor.
use crate::{
    model::AgentKind,
    render::{self, Style},
    report::{AgentActivity, MonitorInfo},
};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

fn rate_label(rate: f64) -> String {
    if rate >= 1_000_000_000_000.0 {
        format!("{rate:.1e}")
    } else if rate >= 10.0 {
        render::number(rate.round() as u64, false)
    } else if rate > 0.0 {
        format!("{rate:.1}")
    } else {
        "0".into()
    }
}

/// Spread historical/live points across the plot without inventing intermediate
/// values. Narrow charts retain the newest points rather than dropping spikes.
pub fn speed_history(
    info: &MonitorInfo,
    style: &Style,
    width: usize,
    rows: usize,
    ascii: bool,
) -> Vec<String> {
    history(
        &info.output_speed_history,
        ChartKind::Logged,
        style,
        width,
        rows,
        ascii,
    )
}

/// One agent's timing estimates with logged throughput where timing is unavailable.
pub fn agent_history(
    activity: &AgentActivity,
    interval: Duration,
    style: &Style,
    width: usize,
    rows: usize,
    ascii: bool,
) -> Vec<String> {
    let rates: Vec<_> = activity
        .estimated_output_speed_history
        .iter()
        .enumerate()
        .map(|(index, estimate)| {
            estimate.or_else(|| {
                activity
                    .fallback_output_speed_history
                    .get(index)
                    .copied()
                    .flatten()
            })
        })
        .collect();
    history(
        &rates,
        ChartKind::Estimated {
            agent: activity.agent,
            interval,
        },
        style,
        width,
        rows,
        ascii,
    )
}

enum ChartKind {
    Logged,
    Estimated {
        agent: AgentKind,
        interval: Duration,
    },
}

fn history(
    history: &[Option<f64>],
    kind: ChartKind,
    style: &Style,
    width: usize,
    rows: usize,
    ascii: bool,
) -> Vec<String> {
    if width < 32 || rows < 2 {
        return Vec::new();
    }
    // Reserve enough room for abbreviated or scientific tick labels at every scale.
    let axis_width = 7;
    let plot_width = width.saturating_sub(axis_width + 2);
    let count = history.len().min(plot_width);
    let samples = &history[history.len() - count..];
    let (peak, average, valid) = samples
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .fold((0.0_f64, 0.0_f64, 0usize), |(peak, mean, count), rate| {
            // Online mean avoids overflowing when large valid values are summed.
            (
                peak.max(rate),
                mean + (rate - mean) / (count + 1) as f64,
                count + 1,
            )
        });
    let (title, compact, minimum) = match kind {
        ChartKind::Logged => ("Logged output tok/s", "Log tok/s", "tok/s"),
        ChartKind::Estimated {
            agent: AgentKind::Codex,
            ..
        } => ("Codex ~output tok/s", "Codex ~tok/s", "Codex ~tok/s"),
        ChartKind::Estimated {
            agent: AgentKind::ClaudeCode,
            ..
        } => ("Claude ~request tok/s", "Claude ~tok/s", "Claude ~tok/s"),
    };
    let estimated = matches!(kind, ChartKind::Estimated { .. });
    let separator = if ascii { " / " } else { " · " };
    let statistics = if valid == 0 {
        format!("avg n/a{separator}peak n/a")
    } else {
        format!(
            "avg {}{separator}peak {}",
            rate_label(average),
            rate_label(peak)
        )
    };
    let last = if estimated { samples } else { history }
        .iter()
        .rev()
        .copied()
        .flatten()
        .find(|rate| rate.is_finite() && *rate > 0.0)
        .map(rate_label)
        .unwrap_or_else(|| "n/a".into());
    let latest = format!("last {last}");
    let full_statistics = format!("{statistics}{separator}{latest}");
    let heading = [
        (title, full_statistics.as_str()),
        (title, statistics.as_str()),
        (compact, full_statistics.as_str()),
        (compact, statistics.as_str()),
        (minimum, statistics.as_str()),
        (title, latest.as_str()),
        (compact, latest.as_str()),
    ]
    .into_iter()
    .find(|(label, stats)| label.width() + stats.width() + 1 <= width)
    .map_or_else(
        || style.paint("meta", title),
        |(label, stats)| {
            format!(
                "{}{}{}",
                style.paint("meta", label),
                " ".repeat(width - label.width() - stats.width()),
                style.paint("accent", stats)
            )
        },
    );
    let scale = peak.max(1.0);
    let bar_rows = rows - 1;
    let mut lines = vec![heading];
    for row in 0..bar_rows {
        let rows_above_zero = bar_rows - row;
        let tick = rate_label(scale * (rows_above_zero as f64 / bar_rows as f64));
        let mut line = style.paint("meta", format!("{tick:>axis_width$}  "));
        for column in 0..plot_width {
            let index = column * count / plot_width;
            let Some(rate) = samples
                .get(index)
                .copied()
                .flatten()
                .filter(|v| v.is_finite() && *v > 0.0)
            else {
                line.push(' ');
                continue;
            };
            let units = ((rate / scale * bar_rows as f64 * 8.0).round() as usize).max(1);
            let fill = units.saturating_sub((rows_above_zero - 1) * 8).min(8);
            let glyph = if ascii && fill > 0 {
                '#'
            } else {
                [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][fill]
            };
            let role = if index + 1 == count { "bold" } else { "accent" };
            line.push_str(&style.paint(role, glyph.to_string()));
        }
        if !estimated && samples.is_empty() && row == rows / 2 {
            line = style.paint("meta", format!("{:>axis_width$}  Waiting for samples", ""));
        }
        lines.push(line);
    }
    // The upper-edge block meets the bottom of the bar glyphs in the preceding row.
    let baseline = if ascii { "-" } else { "▔" }.repeat(plot_width);
    lines.push(style.paint("meta", format!("{:>axis_width$}  {baseline}", 0)));
    let footer = match kind {
        ChartKind::Estimated { interval, .. } => {
            elapsed_axis(interval * count as u32, plot_width, ascii)
        }
        ChartKind::Logged => {
            let left = if count == 1 {
                "1 point".into()
            } else {
                format!("{count} points")
            };
            let gap = plot_width.saturating_sub(left.width() + "latest".len());
            format!("{left}{}latest", " ".repeat(gap))
        }
    };
    lines.push(style.paint("meta", format!("{:>axis_width$}  {footer}", "")));
    lines
}

fn elapsed_axis(span: Duration, width: usize, ascii: bool) -> String {
    if span.is_zero() {
        return format!("{:>width$}", "now");
    }
    let left = elapsed_label(span, ascii);
    let middle = elapsed_label(span / 2, ascii);
    let middle_start = width.saturating_sub(middle.width()) / 2;
    let now_start = width.saturating_sub("now".len());
    let before_middle = middle_start.saturating_sub(left.width());
    let after_middle = now_start.saturating_sub(middle_start + middle.width());
    format!(
        "{left}{}{middle}{}now",
        " ".repeat(before_middle),
        " ".repeat(after_middle)
    )
}

fn elapsed_label(elapsed: Duration, ascii: bool) -> String {
    let minus = if ascii { '-' } else { '−' };
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!("{minus}{}h{:02}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{minus}{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        let seconds = format!("{:.3}", elapsed.as_secs_f64());
        format!(
            "{minus}{}s",
            seconds.trim_end_matches('0').trim_end_matches('.')
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::Cli, model::Scan, monitor::Tracker, report};
    use clap::Parser;

    fn fixture(history: Vec<Option<f64>>) -> (MonitorInfo, Style) {
        let cli = Cli::parse_from(["agent-usage", "monitor", "--no-cost", "--color", "never"]);
        let report = report::build(
            &cli,
            chrono_tz::UTC,
            &Scan::default(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
        let mut info = Tracker::new(&Scan::default(), &report, false).info(0);
        info.output_speed_history = history;
        (info, Style::new(&cli, false))
    }

    #[test]
    fn bars_show_order_scale_zero_and_missing_data_without_fabricating_history() {
        let (info, style) = fixture(vec![Some(0.0), Some(5.0), Some(10.0), None]);
        let lines = speed_history(&info, &style, 33, 4, false);
        assert_eq!(lines.len(), 6);
        assert!(lines[0].contains("avg 5.0") && lines[0].contains("peak 10"));
        assert_eq!(&lines[3][9..], "      ████████████      ");
        assert_eq!(&lines[4][9..], "▔".repeat(24));
        assert!(lines[5].contains("4 points") && lines[5].ends_with("latest"));
        let (empty, style) = fixture(Vec::new());
        let output = speed_history(&empty, &style, 60, 4, false).join("\n");
        assert!(output.starts_with("Logged output tok/s"), "{output}");
        assert!(output.contains("Waiting for samples") && output.contains("0 points"));
        assert!(output.contains("avg n/a") && output.contains("peak n/a"));
        assert!(!output.contains('█'));
        assert!(output.contains("peak n/a · last n/a"));
        let (info, style) = fixture(vec![Some(10.0), Some(2.0), Some(0.0), Some(0.0)]);
        let heading = &speed_history(&info, &style, 80, 4, false)[0];
        assert!(heading.contains("peak 10 · last 2.0"), "{heading}");
        let (info, style) = fixture(vec![Some(10.0), None]);
        let heading = &speed_history(&info, &style, 80, 4, false)[0];
        assert!(heading.contains("peak 10 · last 10"), "{heading}");
    }

    #[test]
    fn narrow_charts_keep_recent_samples_and_fit_all_scales_and_ascii_modes() {
        let (mut info, style) = fixture(vec![Some(1000.0); 60]);
        info.output_speed_history[59] = Some(1.0);
        for width in [32, 40, 80, 140, 260] {
            for rows in [2, 4, 8] {
                for ascii in [false, true] {
                    let lines = speed_history(&info, &style, width, rows, ascii);
                    assert!(lines.iter().all(|line| line.width() <= width), "{lines:?}");
                    assert!(lines[0].contains("tok/s"));
                    assert_eq!(lines[1].chars().nth(9), Some(if ascii { '#' } else { '█' }));
                    assert_eq!(lines[rows].width(), width, "history must fill the plot");
                    if ascii {
                        assert!(lines.iter().all(|line| line.is_ascii()));
                    }
                }
            }
        }
        assert!(speed_history(&info, &style, 31, 4, false).is_empty());
        assert!(speed_history(&info, &style, 80, 1, false).is_empty());
        assert!(speed_history(&info, &style, 32, 4, false)
            .last()
            .unwrap()
            .contains("23 points"));
        assert!(speed_history(&info, &style, 32, 4, false)[0].contains("avg 957"));
        assert!(speed_history(&info, &style, 140, 4, false)[0].contains("avg 983"));
    }

    #[test]
    fn invalid_values_are_gaps_and_tiny_positive_rates_remain_visible() {
        let (info, style) = fixture(vec![
            Some(f64::NAN),
            Some(-1.0),
            Some(f64::MIN_POSITIVE),
            Some(f64::MAX),
        ]);
        let lines = speed_history(&info, &style, 33, 4, false);
        assert!(lines.iter().all(|line| line.width() <= 60));
        assert_eq!(&lines[3][9..], "            ▁▁▁▁▁▁██████");
        assert_eq!(&lines[4][9..], "▔".repeat(24));
        assert!(!lines.join("\n").contains("NaN"));
        assert!(lines[0].contains("avg 9.0e307"), "{lines:?}");
        let (large, style) = fixture(vec![Some(f64::MAX); 60]);
        let heading = speed_history(&large, &style, 32, 4, false).remove(0);
        assert!(
            heading.contains("avg 1.8e308") && !heading.contains("inf"),
            "{heading}"
        );
    }

    #[test]
    fn agent_charts_prefer_timed_rates_and_plot_fallback_over_the_same_axis() {
        let (_, style) = fixture(Vec::new());
        for (agent, name, title) in [
            (AgentKind::Codex, "Codex", "Codex ~output tok/s"),
            (AgentKind::ClaudeCode, "Claude", "Claude ~request tok/s"),
        ] {
            let mut activity = AgentActivity {
                agent,
                last_message_speed: None,
                window_cost_usd: None,
                cost_partial: false,
                average_session_output_tokens_per_second: 0.0,
                since_ready_tokens: 0,
                window_tokens: 0,
                window_output_tokens: 0,
                output_tokens_per_second: 1000.0,
                output_speed_history: vec![Some(1000.0); 4],
                output_speed_window_seconds: 2,
                estimated_output_speed_history: vec![Some(20.0), None, None, Some(40.0)],
                fallback_output_speed_history: vec![Some(120.0), None, Some(24.0), Some(900.0)],
                logged_output_tokens_history: vec![12, 0, 50, 120],
            };
            for ascii in [false, true] {
                let interval = Duration::from_millis(1500);
                let lines = agent_history(&activity, interval, &style, 80, 4, ascii);
                assert!(lines[0].starts_with(title), "{lines:?}");
                assert!(lines[0].contains("avg 28") && lines[0].contains("peak 40"));
                let baseline = if ascii { "-" } else { "▔" };
                assert!(lines[1..4]
                    .iter()
                    .all(|row| { row.chars().skip(27).take(18).all(|cell| cell == ' ') }));
                assert_eq!(
                    lines[3].chars().nth(50),
                    Some(if ascii { '#' } else { '█' })
                );
                assert!(lines[3].ends_with(if ascii { '#' } else { '█' }));
                assert_eq!(&lines[4][9..], baseline.repeat(71));
                let minus = if ascii { '-' } else { '−' };
                let axis = lines.last().unwrap();
                assert!(axis.starts_with(&format!("         {minus}6s")), "{axis:?}");
                assert!(axis.contains(&format!("{minus}3s")), "{axis:?}");
                assert!(axis.ends_with("now"));
                assert_eq!(axis.width(), 80);

                let narrow = agent_history(&activity, interval, &style, 32, 2, ascii);
                assert!(narrow[0].starts_with(name), "{narrow:?}");
                assert!(narrow.iter().all(|line| line.width() <= 32));
                assert!(narrow[1].ends_with(if ascii { '#' } else { '█' }));
                assert_eq!(&narrow[2][9..], baseline.repeat(23));
            }

            activity.estimated_output_speed_history = vec![None; 180];
            activity.fallback_output_speed_history = vec![None; 180];
            activity.logged_output_tokens_history = vec![0; 180];
            let interval = Duration::from_secs(2);
            let empty = agent_history(&activity, interval, &style, 80, 4, false);
            assert_eq!(empty.len(), 6);
            assert!(empty[0].starts_with(title) && empty[0].contains("peak n/a"));
            assert!(empty[1..4]
                .iter()
                .all(|row| { row.width() == 80 && row.chars().skip(9).all(|cell| cell == ' ') }));
            assert_eq!(&empty[4][9..], "▔".repeat(71));
            activity.logged_output_tokens_history[0] = 100;
            activity.logged_output_tokens_history[179] = 50;
            activity.fallback_output_speed_history[0] = Some(100.0);
            activity.fallback_output_speed_history[179] = Some(5.0);
            let untimed = agent_history(&activity, interval, &style, 80, 4, false);
            assert!(untimed[0].contains("avg 5.0") && untimed[0].contains("peak 5.0"));
            assert!(untimed[3].ends_with('█'));
            assert_eq!(untimed[4..], empty[4..]);
            activity.estimated_output_speed_history[179] = Some(40.0);
            let wide = agent_history(&activity, interval, &style, 80, 4, false);
            assert!(wide[0].contains("avg 40") && wide[0].contains("peak 40"));
            assert!(wide[3].ends_with('█'));
            assert_eq!(wide[4], empty[4]);
            let axis = wide.last().unwrap();
            assert_eq!(empty.last(), Some(axis));
            assert!(
                axis.contains("−2m22s") && axis.contains("−1m11s"),
                "{axis:?}"
            );

            activity.estimated_output_speed_history.clear();
            activity.fallback_output_speed_history.clear();
            activity.logged_output_tokens_history.clear();
            let startup = agent_history(&activity, interval, &style, 32, 4, true);
            assert_eq!(startup.len(), 6);
            assert!(startup[1..4]
                .iter()
                .all(|row| { row.width() == 32 && row.chars().skip(9).all(|cell| cell == ' ') }));
            assert_eq!(&startup[4][9..], "-".repeat(23));
            assert_eq!(startup[5].trim_start(), "now");
        }
    }
}
