//! Compact rolling output-speed history for the full-screen monitor.
use crate::{
    render::{self, Style},
    report::MonitorInfo,
};
use unicode_width::UnicodeWidthStr;

fn label(rate: f64) -> String {
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
    if width < 32 || rows < 2 {
        return Vec::new();
    }
    // Reserve enough room for abbreviated or scientific tick labels at every scale.
    let axis_width = 7;
    let plot_width = width.saturating_sub(axis_width + 2);
    let count = info.output_speed_history.len().min(plot_width);
    let samples = &info.output_speed_history[info.output_speed_history.len() - count..];
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
    let scale = peak.max(1.0);
    let separator = if ascii { " / " } else { " · " };
    let title = format!(
        "Output tok/s{separator}{}s avg",
        info.output_speed_window_seconds
    );
    let statistics = if valid == 0 {
        format!("avg n/a{separator}peak n/a")
    } else {
        format!("avg {}{separator}peak {}", label(average), label(peak))
    };
    let title = if title.width() + statistics.width() + 2 <= width {
        title.as_str()
    } else if "Output tok/s".len() + statistics.width() + 2 <= width {
        "Output tok/s"
    } else {
        "tok/s"
    };
    let gap = width.saturating_sub(title.width() + statistics.width());
    let heading = if gap >= 1 {
        format!(
            "{}{}{}",
            style.paint("meta", title),
            " ".repeat(gap),
            style.paint("accent", statistics)
        )
    } else {
        style.paint("meta", title)
    };
    let mut lines = vec![heading];
    for row in 0..rows {
        let tick = label(scale * ((rows - row) as f64 / rows as f64));
        let mut line = style.paint("meta", format!("{tick:>axis_width$}  "));
        for column in 0..if count == 0 { 0 } else { plot_width } {
            let index = column * count / plot_width;
            let sample = samples[index];
            let rate = sample.filter(|v| v.is_finite() && *v >= 0.0);
            let glyph = if let Some(rate) = rate {
                let units = ((rate / scale * rows as f64 * 8.0).round() as usize)
                    .max(usize::from(rate > 0.0));
                let fill = units.saturating_sub((rows - row - 1) * 8).min(8);
                if fill == 0 {
                    if row == rows - 1 && rate == 0.0 {
                        if ascii {
                            '.'
                        } else {
                            '·'
                        }
                    } else {
                        ' '
                    }
                } else if ascii {
                    '#'
                } else {
                    [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][fill]
                }
            } else if row == rows - 1 {
                '?'
            } else {
                ' '
            };
            let role = if rate.is_none() {
                "warn"
            } else if index + 1 == count {
                "bold"
            } else {
                "accent"
            };
            line.push_str(&style.paint(role, glyph.to_string()));
        }
        if samples.is_empty() && row == rows / 2 {
            line = style.paint("meta", format!("{:>axis_width$}  Waiting for samples", ""));
        }
        lines.push(line);
    }
    let left = if count == 1 {
        "1 point".into()
    } else {
        format!("{count} points")
    };
    let gap = plot_width.saturating_sub(left.width() + "latest".len());
    lines.push(style.paint(
        "meta",
        format!("{:>axis_width$}  {left}{}latest", "0", " ".repeat(gap)),
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::Cli, model::Scan, monitor::Tracker, report};
    use clap::Parser;

    fn fixture(history: Vec<Option<f64>>) -> (MonitorInfo, Style) {
        let cli = Cli::parse_from(["codex-usage", "monitor", "--no-cost", "--color", "never"]);
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
        assert!(lines[4].ends_with("······████████████??????"), "{lines:?}");
        assert!(lines[5].contains("4 points") && lines[5].ends_with("latest"));
        let (empty, style) = fixture(Vec::new());
        let output = speed_history(&empty, &style, 60, 4, false).join("\n");
        assert!(output.contains("Waiting for samples") && output.contains("0 points"));
        assert!(output.contains("avg n/a") && output.contains("peak n/a"));
        assert!(!output.contains('█'));
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
        assert!(lines[4].ends_with("????????????▁▁▁▁▁▁██████"), "{lines:?}");
        assert!(!lines.join("\n").contains("NaN"));
        assert!(lines[0].contains("avg 9.0e307"), "{lines:?}");
        let (large, style) = fixture(vec![Some(f64::MAX); 60]);
        let heading = speed_history(&large, &style, 32, 4, false).remove(0);
        assert!(
            heading.contains("avg 1.8e308") && !heading.contains("inf"),
            "{heading}"
        );
    }
}
