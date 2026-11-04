use crate::{
    cli::{Cli, Pager},
    render::{self, Style},
    report::Report,
};
use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub fn interactive() -> bool {
    io::stdout().is_terminal()
        && io::stdin().is_terminal()
        && std::env::var("TERM").unwrap_or_default() != "dumb"
}
struct Screen;
struct StopCollector(Arc<AtomicBool>);
impl Drop for StopCollector {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
impl Screen {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
fn size() -> (usize, usize) {
    let (w, h) = terminal::size().unwrap_or((100, 24));
    (w.saturating_sub(1).max(1) as usize, h.max(1) as usize)
}
fn text(lines: &mut Vec<String>, style: &Style, value: &str, width: usize) {
    lines.extend(
        render::wrap(value, width)
            .iter()
            .map(|s| style.paint("meta", s)),
    );
}
fn nav(
    style: &Style,
    width: usize,
    a: usize,
    b: usize,
    total: usize,
    chart_control: bool,
) -> Vec<String> {
    let mut v = Vec::new();
    text(
        &mut v,
        style,
        &format!(
            "{a}-{b}/{total}{}  q Quit  ? Help",
            if chart_control { "  c Chart" } else { "" }
        ),
        width,
    );
    v
}

/// Read-only reports and the live screen share navigation and terminal cleanup.
/// The refresh callback runs only for monitor, with all source/accounting state owned outside UI.
pub fn run<F>(
    cli: &Cli,
    mut report: Report,
    stopped: Arc<AtomicBool>,
    mut refresh: F,
) -> Result<Report>
where
    F: FnMut() -> Result<Report> + Send,
{
    let tty = interactive();
    let (width, height) = size();
    let style = Style::new(cli, tty);
    let first = render::view(cli, &report, &style, width, false);
    let use_screen = tty
        && (cli.monitor()
            || cli.pager == Pager::Always
            || (cli.pager == Pager::Auto
                && first.header.len() + first.body.len() + first.footer.len() + 1 > height));
    if !use_screen {
        let width = if io::stdout().is_terminal() {
            width
        } else {
            120
        };
        let mut out = io::stdout().lock();
        for line in render::linear(cli, &report, width, io::stdout().is_terminal()) {
            writeln!(out, "{line}")?;
        }
        out.flush()?;
        return Ok(report);
    }
    std::thread::scope(|scope| -> Result<Report> {
        let (work_tx, work_rx) = std::sync::mpsc::channel::<()>();
        let (results_tx, results_rx) = std::sync::mpsc::channel::<Result<Report>>();
        // Drop after the screen guard, before the scoped collector is joined.
        let _stop_collector = StopCollector(Arc::clone(&stopped));
        if cli.monitor() {
            scope.spawn(move || {
                while work_rx.recv().is_ok() {
                    if results_tx.send(refresh()).is_err() {
                        break;
                    }
                }
            });
        }
        let guard = Screen::enter()?;
        let mut offset = 0usize;
        let mut help = false;
        let mut show_chart = cli.monitor();
        let mut saved_offset = 0;
        let mut previous: Vec<String> = Vec::new();
        let mut old_identities: Vec<(String, usize)> = Vec::new();
        let mut old_width = 0;
        let mut last_size = (0, 0);
        let mut next = Instant::now() + cli.interval();
        let mut reanchor = false;
        let mut pending = false;
        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(result) = results_rx.try_recv() {
                match result {
                    Ok(next_report) => report = next_report,
                    Err(error) if crate::cancel::is_cancelled(&error) => break,
                    Err(error) => return Err(error),
                }
                pending = false;
                reanchor = true;
            }
            let (width, height) = size();
            let mut v = render::view(cli, &report, &style, width, help);
            // Preserve the top logical identity through reflow or report reordering.
            let anchor = old_identities
                .iter()
                .rev()
                .find(|(_, line)| *line <= offset)
                .map(|(id, _)| id);
            if old_width != width || reanchor {
                if let Some(id) = anchor {
                    if let Some((_, line)) = v.identities.iter().find(|(name, _)| name == id) {
                        offset = *line;
                    }
                }
            }
            old_width = width;
            reanchor = false;
            let mut reserve_nav = nav(
                &style,
                width,
                v.body.len(),
                v.body.len(),
                v.body.len(),
                cli.monitor(),
            );
            let mut footer_size = v.footer.len() + reserve_nav.len();
            let date_rows = v
                .sections
                .iter()
                .map(|(label, _)| render::wrap(label, width).len())
                .max()
                .unwrap_or(0);
            let room = height.saturating_sub(v.header.len() + footer_size + date_rows);
            let chart_available = cli.monitor() && !help && width >= 32 && room >= 9;
            if !chart_available && cli.monitor() {
                reserve_nav = nav(
                    &style,
                    width,
                    v.body.len(),
                    v.body.len(),
                    v.body.len(),
                    false,
                );
                footer_size = v.footer.len() + reserve_nav.len();
            }
            if show_chart && chart_available {
                if let Some(info) = &report.monitor {
                    let chart = crate::chart::speed_history(
                        info,
                        &style,
                        width,
                        (room / 3).clamp(2, 18).min(room - 7),
                        cli.ascii,
                    );
                    if !chart.is_empty() {
                        v.header.splice(
                            v.chart_anchor..v.chart_anchor,
                            std::iter::once(String::new()).chain(chart),
                        );
                    }
                }
            }
            let chrome = v.header.len() + footer_size + date_rows;
            let page = height.saturating_sub(chrome).max(1);
            offset = offset.min(v.body.len().saturating_sub(page));
            let mut lines = if chrome >= height {
                let mut lines = Vec::new();
                text(&mut lines, &style, "Resize terminal / q Quit", width);
                lines
            } else {
                let mut lines = v.header;
                let mut context = Vec::new();
                if let Some((label, line)) =
                    v.sections.iter().rev().find(|(_, line)| *line <= offset)
                {
                    // Pin a date only after its original heading has scrolled away.
                    if line + render::wrap(label, width).len() <= offset {
                        context = render::wrap(label, width)
                            .iter()
                            .map(|s| style.paint("accent", s))
                            .collect();
                    }
                }
                context.resize(date_rows, String::new());
                lines.extend(context);
                lines.extend(v.body.iter().skip(offset).take(page).cloned());
                while lines.len() < height - footer_size {
                    lines.push(String::new());
                }
                lines.extend(v.footer);
                let mut navigation = nav(
                    &style,
                    width,
                    offset + 1,
                    (offset + page).min(v.body.len()),
                    v.body.len(),
                    chart_available,
                );
                while navigation.len() < reserve_nav.len() {
                    navigation.push(String::new());
                }
                lines.extend(navigation);
                lines
            };
            lines.truncate(height);
            let mut out = io::stdout().lock();
            if last_size != (width, height) {
                queue!(out, Clear(ClearType::All))?;
                previous.clear();
                last_size = (width, height);
            }
            for (y, line) in lines.iter().enumerate() {
                if previous.get(y) != Some(line) {
                    queue!(
                        out,
                        MoveTo(0, y as u16),
                        Clear(ClearType::CurrentLine),
                        Print(line)
                    )?;
                }
            }
            for y in lines.len()..previous.len().min(height) {
                queue!(out, MoveTo(0, y as u16), Clear(ClearType::CurrentLine))?;
            }
            out.flush()?;
            drop(out);
            previous = lines;
            old_identities = v.identities;
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let timeout = if cli.monitor() && !pending {
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
            } else {
                Duration::from_millis(100)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Char('c') if chart_available => {
                            if offset.saturating_add(page) >= v.body.len() {
                                offset = usize::MAX;
                            }
                            show_chart = !show_chart;
                            previous.clear();
                            reanchor = true;
                        }
                        KeyCode::Char('?') => {
                            help = !help;
                            std::mem::swap(&mut offset, &mut saved_offset);
                            previous.clear();
                            old_identities.clear();
                        }
                        KeyCode::Down | KeyCode::Char('j') => offset = offset.saturating_add(1),
                        KeyCode::Up | KeyCode::Char('k') => offset = offset.saturating_sub(1),
                        KeyCode::PageDown | KeyCode::Char(' ') => {
                            offset = offset.saturating_add(page)
                        }
                        KeyCode::PageUp => offset = offset.saturating_sub(page),
                        KeyCode::Home | KeyCode::Char('g') => offset = 0,
                        KeyCode::End | KeyCode::Char('G') => {
                            offset = v.body.len().saturating_sub(page)
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => previous.clear(),
                    _ => {}
                }
            }
            if cli.monitor() && !pending && Instant::now() >= next {
                if cli
                    .frames()
                    .is_some_and(|n| report.monitor.as_ref().is_some_and(|m| m.sequence + 1 >= n))
                {
                    break;
                }
                work_tx.send(())?;
                next = Instant::now() + cli.interval();
                pending = true;
            }
        }
        drop(guard);
        let suffix = if cli.no_cost {
            String::new()
        } else {
            format!(" / {} est.", render::money(report.totals.cost_usd))
        };
        println!(
            "{} / {} / {} tokens{}",
            report.kind,
            report.generated_at.format("%Y-%m-%d %H:%M:%S UTC"),
            render::integer(report.totals.total),
            suffix
        );
        Ok(report)
    })
}
