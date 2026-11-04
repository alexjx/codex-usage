//! Interactive synthetic UI demonstration. Never reads session data.
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    collections::VecDeque,
    env,
    io::{self, IsTerminal, Write},
    process,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

struct Options {
    color: Option<bool>,
    theme: String,
    monitor: bool,
    long: bool,
    ascii: bool,
    pager: String,
    interval: Duration,
    frames: Option<u64>,
    width: Option<u16>,
    height: Option<u16>,
}

impl Options {
    fn parse() -> Result<Option<Self>, String> {
        let mut o = Self {
            color: None,
            theme: "terminal".into(),
            monitor: false,
            long: false,
            ascii: false,
            pager: "auto".into(),
            interval: Duration::from_secs(1),
            frames: None,
            width: None,
            height: None,
        };
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--monitor" => o.monitor = true,
                "--long" => o.long = true,
                "--ascii" => o.ascii = true,
                "--help" | "-h" => {
                    println!("Codex Usage UI demo (synthetic data; no sessions read)\n\n--long                  360 daily/model rows\n--monitor               Live synthetic updates; q or Ctrl+C stops\n--interval 1s|200ms      Demo tick interval (minimum 50ms)\n--frames N              Stop after N snapshots\n--pager auto|always|never\n--color auto|always|never\n--theme terminal|dark|light\n--ascii\n--width N --height N    Override layout dimensions for demonstrations\n\nViewer: arrows/j/k, PgUp/PgDn, Home/End, g/G, q/Ctrl+C");
                    return Ok(None);
                }
                _ => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    match arg.as_str() {
                        "--color" => {
                            o.color = match value.as_str() {
                                "auto" => None,
                                "always" => Some(true),
                                "never" => Some(false),
                                _ => return Err("--color requires auto, always, or never".into()),
                            }
                        }
                        "--theme" if ["terminal", "dark", "light"].contains(&value.as_str()) => {
                            o.theme = value
                        }
                        "--pager" if ["auto", "always", "never"].contains(&value.as_str()) => {
                            o.pager = value
                        }
                        "--interval" => {
                            let millis = if let Some(v) = value.strip_suffix("ms") {
                                v.parse::<u64>().map_err(|_| "invalid milliseconds")?
                            } else if let Some(v) = value.strip_suffix('s') {
                                v.parse::<u64>()
                                    .map_err(|_| "invalid seconds")?
                                    .checked_mul(1000)
                                    .ok_or("interval too large")?
                            } else {
                                return Err(
                                    "--interval requires an integer with ms or s suffix".into()
                                );
                            };
                            if !(50..=60_000).contains(&millis) {
                                return Err("demo interval must be 50ms through 60s".into());
                            }
                            o.interval = Duration::from_millis(millis);
                        }
                        "--frames" => {
                            o.frames = Some(
                                value
                                    .parse::<u64>()
                                    .ok()
                                    .filter(|v| (1..=100_000).contains(v))
                                    .ok_or("--frames must be 1..100000")?,
                            )
                        }
                        "--width" => {
                            o.width = Some(
                                value
                                    .parse::<u16>()
                                    .ok()
                                    .filter(|v| *v >= 20)
                                    .ok_or("--width must be 20..65535")?,
                            )
                        }
                        "--height" => {
                            o.height = Some(
                                value
                                    .parse::<u16>()
                                    .ok()
                                    .filter(|v| *v >= 10)
                                    .ok_or("--height must be 10..65535")?,
                            )
                        }
                        _ => return Err(format!("invalid option or value: {arg} {value}")),
                    }
                }
            }
        }
        if !o.monitor && o.frames.is_some() {
            return Err("--frames requires --monitor".into());
        }
        Ok(Some(o))
    }
}

struct Style {
    color: bool,
    theme: String,
}
impl Style {
    fn paint(&self, role: &str, value: impl AsRef<str>) -> String {
        let value = value.as_ref();
        if !self.color {
            return value.into();
        }
        let code = match (self.theme.as_str(), role) {
            (_, "bold") => "1",
            ("dark", "accent") => "38;2;103;212;232",
            ("dark", "cost") => "38;2;196;181;253",
            ("dark", "ok") => "38;2;134;211;165",
            ("dark", "warn") => "38;2;240;198;116",
            ("dark", _) => "38;2;168;179;199",
            ("light", "accent") => "38;2;0;103;125",
            ("light", "cost") => "38;2;107;63;160",
            ("light", "ok") => "38;2;23;100;61",
            ("light", "warn") => "38;2;128;86;0",
            ("light", _) => "38;2;82;96;117",
            (_, "accent") => "36",
            (_, "cost") => "35",
            (_, "ok") => "32",
            (_, "warn") => "33",
            _ => "0",
        };
        format!("\x1b[{code}m{value}\x1b[0m")
    }
}

#[derive(Clone)]
struct Row {
    id: String,
    input: u64,
    cached: u64,
    output: u64,
    cost: Option<u64>,
}
impl Row {
    fn total(&self) -> u64 {
        self.input + self.output
    }
}

fn rows(o: &Options, tick: u64) -> Vec<Row> {
    let mut result = Vec::new();
    let count = if o.long { 360 } else { 3 };
    for i in 0..count {
        let index = i % 3;
        let (name, input, cached, output, cost) = match index {
            0 => ("model-alpha", 100_000, 60_000, 20_000, Some(380_000_000)),
            1 => ("model-beta", 50_000, 10_000, 10_000, Some(90_000_000)),
            _ => ("unknown", 2000, 0, 500, None),
        };
        let id = if o.long && !o.monitor {
            let day = 120 - i / 3;
            // Explicit synthetic day indexes avoid pretending to implement a date library.
            format!("day-{day:03} / {name}")
        } else if o.long {
            format!("{name}-{:03}", i / 3 + 1)
        } else {
            name.into()
        };
        let advances = if i < 3 { tick } else { 0 };
        // Fictional request: +1000 uncached input, +200 output for each priced model.
        // Integer nanodollars retain the fractional fourth decimal until display.
        let increment = match index {
            0 => 5_000_000,
            1 => 1_960_000,
            _ => 0,
        };
        result.push(Row {
            id,
            input: input + advances * 1000,
            cached,
            output: output + advances * 200,
            cost: cost.map(|v| v + advances * increment),
        });
    }
    result
}

fn number(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
fn money(value: u64) -> String {
    let display = (value + 50_000) / 100_000;
    format!("${}.{:04}", display / 10000, display % 10000)
}

// Wrap raw text before styling: ANSI bytes never count toward cell width.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if cells + size > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            cells = 0;
        }
        line.push(c);
        cells += size;
    }
    lines.push(line);
    lines
}
fn text(lines: &mut Vec<String>, style: &Style, role: &str, raw: &str, width: usize) {
    lines.extend(wrap(raw, width).iter().map(|part| style.paint(role, part)));
}

struct View {
    header: Vec<String>,
    body: Vec<String>,
    footer: Vec<String>,
    row_starts: Vec<usize>,
}

#[derive(Default)]
struct Speed {
    samples: VecDeque<(Duration, u64)>,
}
impl Speed {
    fn record(&mut self, time: Duration, output: u64) {
        self.samples.push_back((time, output));
    }
    fn rate(&mut self, now: Duration) -> f64 {
        let window = Duration::from_secs(5);
        if now >= window {
            while self
                .samples
                .front()
                .is_some_and(|(time, _)| *time <= now - window)
            {
                self.samples.pop_front();
            }
        }
        let seconds = now.min(window).as_secs_f64();
        if seconds == 0.0 {
            return 0.0;
        }
        self.samples.iter().map(|(_, tokens)| *tokens).sum::<u64>() as f64 / seconds
    }
}

fn view(o: &Options, style: &Style, data: &[Row], tick: u64, width: usize, speed: f64) -> View {
    let mut h = Vec::new();
    let mut b = Vec::new();
    let mut f = Vec::new();
    let mut row_starts = Vec::new();
    let sep = if o.ascii { " / " } else { " · " };
    let mode = if o.monitor {
        "Monitor / LIVE / DEMO"
    } else if o.long {
        "Daily / 120 days / DEMO"
    } else {
        "Models / DEMO"
    };
    text(
        &mut h,
        style,
        "accent",
        &format!("Codex Usage  /  {mode}"),
        width,
    );
    let stale = o.monitor && tick % 8 >= 4 && tick % 8 <= 5;
    text(
        &mut h,
        style,
        if stale { "warn" } else { "ok" },
        &format!(
            "wsl ready{sep}windows {}",
            if stale { "stale (retrying)" } else { "ready" }
        ),
        width,
    );
    let total: u64 = data.iter().map(Row::total).sum();
    let price: u64 = data.iter().filter_map(|r| r.cost).sum();
    let summary = format!(
        "{} tokens{sep}{} est. subtotal",
        number(total),
        money(price)
    );
    text(&mut h, style, "cost", &summary, width);
    if o.monitor {
        text(
            &mut h,
            style,
            "accent",
            &format!(
                "+{} tokens / +{} / {speed:.0} out tok/s",
                number(tick * 3600),
                money(tick * 6_960_000)
            ),
            width,
        );
    }
    let id_width = data.iter().map(|r| r.id.len()).max().unwrap_or(18).max(18);
    let detailed = width >= id_width + 56;
    let stacked = width < id_width + 27;
    let heading = if detailed {
        format!(
            "{:<id_width$}  {:>9}  {:>9}  {:>8}  {:>9}  {:>10}",
            "Model", "Input", "Cached", "Output", "Total", "Est. USD"
        )
    } else if !stacked {
        format!(
            "{:<id_width$}  {:>12}  {:>10}",
            "Model", "Tokens", "Est. USD"
        )
    } else {
        "Model / Tokens / Est. USD".into()
    };
    h.push(String::new());
    text(&mut h, style, "meta", &heading, width);
    for row in data {
        row_starts.push(b.len());
        let price = row.cost.map(money).unwrap_or("n/a".into());
        if stacked {
            text(&mut b, style, "bold", &row.id, width);
            text(
                &mut b,
                style,
                "cost",
                &format!("{} tokens / {price}", number(row.total())),
                width,
            );
            b.push(String::new());
        } else {
            let identity = style.paint("bold", format!("{:<id_width$}", row.id));
            let lead = if detailed {
                format!(
                    "{}  {:>9}  {:>9}  {:>8}  {:>9}  ",
                    identity,
                    number(row.input),
                    number(row.cached),
                    number(row.output),
                    number(row.total())
                )
            } else {
                format!("{}  {:>12}  ", identity, number(row.total()))
            };
            b.push(format!(
                "{}{}",
                lead,
                style.paint(
                    if row.cost.is_some() { "cost" } else { "warn" },
                    format!("{price:>10}")
                )
            ));
        }
    }
    let priced: u64 = data
        .iter()
        .filter(|r| r.cost.is_some())
        .map(Row::total)
        .sum();
    if priced < total {
        text(
            &mut f,
            style,
            "warn",
            &format!("Unpriced: {} tokens", number(total - priced)),
            width,
        );
    }
    View {
        header: h,
        body: b,
        footer: f,
        row_starts,
    }
}

fn navigation(
    style: &Style,
    width: usize,
    start: usize,
    end: usize,
    total: usize,
    help: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    text(
        &mut lines,
        style,
        "meta",
        &format!("{start}-{end}/{total}  q Quit  ? Help"),
        width,
    );
    if help {
        for line in [
            "Arrows/j/k Scroll / PgUp/PgDn Page / Home/End / Ctrl+C Quit",
            "Totals cover all rows. Cached is included in input.",
            "DEMO: fictional usage and API-equivalent rates; no sessions read.",
            "Speed: logged output tokens over the last 5s; zero when idle.",
        ] {
            text(&mut lines, style, "meta", line, width);
        }
    }
    lines
}

#[derive(Default)]
struct Screen;
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

fn dimensions(o: &Options) -> (usize, usize) {
    let (w, h) = terminal::size().unwrap_or((100, 24));
    (
        o.width.unwrap_or(w).min(w).saturating_sub(1).max(1) as usize,
        o.height.unwrap_or(h).min(h).max(1) as usize,
    )
}

fn run(o: Options) -> io::Result<()> {
    let tty = io::stdout().is_terminal()
        && io::stdin().is_terminal()
        && env::var("TERM").unwrap_or_default() != "dumb";
    let style = Style {
        color: o.color.unwrap_or_else(|| automatic_color(tty)),
        theme: o.theme.clone(),
    };
    let (width, height) = dimensions(&o);
    let data = rows(&o, 0);
    let initial = view(&o, &style, &data, 0, width, 0.0);
    let started = Instant::now();
    let mut speed = Speed::default();
    let viewer = tty
        && (o.monitor
            || o.pager == "always"
            || (o.pager == "auto"
                && initial.header.len() + initial.body.len() + initial.footer.len() + 2 > height));
    if !viewer {
        let frames = if o.monitor {
            o.frames.unwrap_or(u64::MAX)
        } else {
            1
        };
        for tick in 0..frames {
            if tick > 0 {
                std::thread::sleep(o.interval);
                speed.record(started.elapsed(), 600);
            }
            let width = if tty {
                dimensions(&o).0
            } else {
                o.width.unwrap_or(100).saturating_sub(1) as usize
            };
            let snapshot = view(
                &o,
                &style,
                &rows(&o, tick),
                tick,
                width,
                speed.rate(started.elapsed()),
            );
            let mut out = io::stdout().lock();
            for line in snapshot
                .header
                .into_iter()
                .chain(snapshot.body)
                .chain(snapshot.footer)
            {
                writeln!(out, "{line}")?;
            }
            out.flush()?;
        }
        return Ok(());
    }
    let _screen = Screen::enter()?;
    let mut tick = 0;
    let mut offset = 0usize;
    let mut next_tick = Instant::now() + o.interval;
    let mut previous: Vec<String> = Vec::new();
    let mut last_size = (0, 0);
    let mut last_starts = Vec::new();
    let mut help = false;
    loop {
        let (width, height) = dimensions(&o);
        let v = view(
            &o,
            &style,
            &rows(&o, tick),
            tick,
            width,
            speed.rate(started.elapsed()),
        );
        if last_size.0 != width && !last_starts.is_empty() {
            let anchor = last_starts
                .partition_point(|line| *line <= offset)
                .saturating_sub(1);
            offset = v.row_starts.get(anchor).copied().unwrap_or(0);
        }
        let mut footer = v.footer.clone();
        let last_line = v.body.len();
        footer.extend(navigation(
            &style, width, last_line, last_line, last_line, help,
        ));
        let fixed = v.header.len() + footer.len();
        let reserved_footer = footer.len();
        let page = height.saturating_sub(fixed).max(1);
        offset = offset.min(v.body.len().saturating_sub(page));
        let mut lines = if height <= fixed {
            let mut small = Vec::new();
            text(
                &mut small,
                &style,
                "warn",
                "Terminal too small. Resize or q to quit.",
                width,
            );
            small
        } else {
            let mut lines = v.header;
            lines.extend(v.body.iter().skip(offset).take(page).cloned());
            while lines.len() < height.saturating_sub(footer.len()) {
                lines.push(String::new());
            }
            // Position replaces the same-height footer status instead of overflowing it.
            footer = v.footer;
            footer.extend(navigation(
                &style,
                width,
                offset + 1,
                (offset + page).min(v.body.len()),
                v.body.len(),
                help,
            ));
            while footer.len() < reserved_footer {
                footer.push(String::new());
            }
            lines.extend(footer);
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
        // Cursor is hidden, and the last column is reserved to avoid auto-wrap.
        out.flush()?;
        drop(out);
        previous = lines;
        last_starts = v.row_starts;
        let delay = if o.monitor {
            next_tick
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100))
        } else {
            Duration::from_millis(100)
        };
        if event::poll(delay)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('?') => help = !help,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Down | KeyCode::Char('j') => offset = offset.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => offset = offset.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => offset = offset.saturating_add(page),
                    KeyCode::PageUp => offset = offset.saturating_sub(page),
                    KeyCode::Home | KeyCode::Char('g') => offset = 0,
                    KeyCode::End | KeyCode::Char('G') => offset = v.body.len().saturating_sub(page),
                    _ => {}
                },
                Event::Resize(_, _) => previous.clear(),
                _ => {}
            }
        }
        if o.monitor && Instant::now() >= next_tick {
            if o.frames.is_some_and(|n| tick + 1 >= n) {
                break;
            }
            tick += 1;
            speed.record(started.elapsed(), 600);
            next_tick = Instant::now() + o.interval;
        }
    }
    drop(_screen);
    let final_data = rows(&o, tick);
    println!(
        "{} closed / {} snapshots / {} tokens / {} priced subtotal (synthetic)",
        if o.monitor {
            "Monitor demo"
        } else {
            "Report viewer"
        },
        tick + 1,
        number(final_data.iter().map(Row::total).sum()),
        money(final_data.iter().filter_map(|r| r.cost).sum())
    );
    Ok(())
}

fn automatic_color(tty: bool) -> bool {
    tty && env::var_os("NO_COLOR").is_none()
}
fn main() {
    match Options::parse() {
        Ok(Some(options)) => {
            if let Err(error) = run(options) {
                if error.kind() != io::ErrorKind::BrokenPipe {
                    eprintln!("UI preview: {error}");
                    process::exit(1);
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> Options {
        Options {
            color: Some(false),
            theme: "terminal".into(),
            monitor: false,
            long: false,
            ascii: true,
            pager: "auto".into(),
            interval: Duration::from_secs(1),
            frames: None,
            width: None,
            height: None,
        }
    }
    #[test]
    fn long_report_keeps_every_row_and_total() {
        let mut o = options();
        o.long = true;
        let data = rows(&o, 0);
        assert_eq!(data.len(), 360);
        assert_eq!(data.first().unwrap().id, "day-120 / model-alpha");
        assert_eq!(data.last().unwrap().id, "day-001 / unknown");
        assert_eq!(data.iter().map(Row::total).sum::<u64>(), 182_500 * 120);
    }
    #[test]
    fn live_cost_rounds_after_aggregation() {
        let o = options();
        let baseline = rows(&o, 0);
        let updated = rows(&o, 1);
        assert_eq!(
            updated.iter().map(Row::total).sum::<u64>()
                - baseline.iter().map(Row::total).sum::<u64>(),
            3600
        );
        let total: u64 = updated.iter().filter_map(|r| r.cost).sum();
        assert_eq!(total, 476_960_000);
        assert_eq!(money(total), "$0.4770");
        assert!(updated.last().unwrap().cost.is_none());
    }
    #[test]
    fn narrow_layout_preserves_identity_and_cell_bounds() {
        let mut o = options();
        o.long = true;
        let style = Style {
            color: false,
            theme: "terminal".into(),
        };
        for width in [20, 39, 59, 79, 99, 139] {
            let v = view(&o, &style, &rows(&o, 0), 0, width, 0.0);
            assert_eq!(v.row_starts.len(), 360);
            assert!(v
                .header
                .iter()
                .chain(v.body.iter())
                .chain(v.footer.iter())
                .all(|line| line.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() <= width));
            assert!(v.body.join("").contains("day-001 / unknown"));
        }
        assert_eq!(wrap("模型-alpha", 4), vec!["模型", "-alp", "ha"]);
    }

    #[test]
    fn output_speed_uses_elapsed_time_and_expires_when_idle() {
        let mut speed = Speed::default();
        assert_eq!(speed.rate(Duration::ZERO), 0.0);
        speed.record(Duration::from_secs(1), 600);
        assert_eq!(speed.rate(Duration::from_secs(1)), 600.0);
        assert_eq!(speed.rate(Duration::from_secs(2)), 300.0);
        assert_eq!(speed.rate(Duration::from_secs(6)), 0.0);
    }
}
