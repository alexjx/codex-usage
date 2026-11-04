use anyhow::{bail, Result};
use chrono::Utc;
use chrono_tz::Tz;
use codex_usage::{
    cancel,
    cli::{Cli, Command, Format, SourceCommand},
    config::{self, Config},
    conversation_ui, export, history,
    model::{Scan, Source},
    monitor::Tracker,
    pricing::Pricer,
    reader::Reader,
    render,
    report::{self, Report},
    ui,
};
use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

fn fail(error: anyhow::Error, code: i32) -> ! {
    if cancel::is_cancelled(&error) {
        eprintln!("Interrupted");
        std::process::exit(130);
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    }) {
        std::process::exit(0);
    }
    eprintln!("codex-usage: {error:#}");
    std::process::exit(code)
}
fn main() {
    let cli = Cli::parse_cli();
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stopped);
    if let Err(error) = ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)) {
        fail(error.into(), 1);
    }
    let path = config::config_path(cli.config.as_deref()).unwrap_or_else(|e| fail(e, 2));
    let cli = history::resolve(cli, &path).unwrap_or_else(|e| fail(e, 2));
    if cli.conversation && !cli.conversation() {
        fail(
            anyhow::anyhow!("--conversation requires session SESSION_ID"),
            2,
        );
    }
    let mut config = Config::load(&path).unwrap_or_else(|e| fail(e, 2));
    if matches!(cli.command, Some(Command::Sources { .. })) {
        if let Err(error) = source_command(&cli, &mut config, &path) {
            fail(error, 2);
        }
        return;
    }
    if matches!(cli.command, Some(Command::Pricing)) {
        if let Err(error) = pricing_command(&cli) {
            fail(error, 2);
        }
        return;
    }
    let selected = config
        .select(&cli.sources, &cli.roots)
        .unwrap_or_else(|e| fail(e, 2));
    let timezone = cli
        .timezone
        .clone()
        .or(config.timezone)
        .unwrap_or_else(local_timezone);
    let tz: Tz = timezone
        .parse()
        .unwrap_or_else(|_| fail(anyhow::anyhow!("unknown timezone {timezone}"), 2));
    report::Period::make(&cli, tz, Utc::now()).unwrap_or_else(|e| fail(e, 2));
    let format = cli
        .format
        .unwrap_or(if cli.monitor() && !ui::interactive() {
            Format::Jsonl
        } else {
            Format::Table
        });
    if cli.monitor() && !matches!(format, Format::Table | Format::Jsonl) {
        fail(
            anyhow::anyhow!("monitor supports --format table or jsonl"),
            2,
        );
    }
    if !cli.monitor() && format == Format::Jsonl {
        fail(
            anyhow::anyhow!("JSONL is for monitor; use --format json for a report"),
            2,
        );
    }
    if cli.conversation() && format == Format::Csv {
        fail(
            anyhow::anyhow!(
                "conversation supports --format table or json; CSV is for usage reports"
            ),
            2,
        );
    }
    let pricer = if cli.no_cost {
        None
    } else {
        Some(load_pricer(&cli).unwrap_or_else(|e| fail(e, 2)))
    };
    if let Err(error) = run(&cli, tz, &selected, pricer.as_ref(), format, stopped) {
        fail(error, 1);
    }
    if let Err(error) = history::save(&cli, &path) {
        eprintln!("Could not remember report options: {error:#}");
    }
}
fn local_timezone() -> String {
    // chrono uses the platform's historical timezone rules through the IANA zone database.
    std::env::var("TZ")
        .ok()
        .filter(|s| s.parse::<Tz>().is_ok())
        .or_else(|| iana_time_zone::get_timezone().ok())
        .unwrap_or("UTC".into())
}
fn source_command(cli: &Cli, config: &mut Config, path: &Path) -> Result<()> {
    let Some(Command::Sources { action }) = &cli.command else {
        unreachable!()
    };
    match action {
        Some(SourceCommand::Set { name, kind }) => {
            if cli.roots.len() != 1 {
                bail!("sources set requires exactly one --root PATH");
            }
            config.set_source(name, kind.as_deref(), &cli.roots[0])?;
            config.save(path)?;
            let source = config.sources.iter().find(|s| s.name == *name).unwrap();
            println!(
                "{} -> {} ({})",
                source.name,
                source.root.display(),
                if source.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if !source.root.is_dir() {
                eprintln!("Path is currently unavailable; saved for later use.");
            }
        }
        Some(SourceCommand::Enable { name }) | Some(SourceCommand::Disable { name }) => {
            let enabled = matches!(action, Some(SourceCommand::Enable { .. }));
            config.set_enabled(name, enabled)?;
            config.save(path)?;
            println!("{name} {}", if enabled { "enabled" } else { "disabled" });
        }
        None => {
            let catalog = config.source_catalog()?;
            let configured = catalog.sources;
            let candidates = catalog.candidates;
            if cli.format == Some(Format::Json) {
                serde_json::to_writer_pretty(
                    io::stdout(),
                    &serde_json::json!({"config":path,"sources":configured,"candidates":candidates}),
                )?;
                println!();
            } else {
                println!("Sources / {}", path.display());
                for source in &configured {
                    println!(
                        "{:<18} {:<8} {:<8} {}",
                        source.name,
                        source.kind,
                        if source.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        source.root.display()
                    );
                }
                if !candidates.is_empty() {
                    println!("\nAvailable (enable to configure)");
                    for candidate in candidates {
                        println!("{:<18} {}", candidate.name, candidate.root.display());
                    }
                }
            }
        }
    }
    Ok(())
}
fn pricing_command(cli: &Cli) -> Result<()> {
    let metadata = load_pricer(cli)?.metadata();
    if cli.format == Some(Format::Json) {
        serde_json::to_writer_pretty(io::stdout(), &metadata)?;
        println!();
        return Ok(());
    }
    println!(
        "Pricing / verified {} / USD per 1M tokens",
        metadata["verified"].as_str().unwrap_or("unknown")
    );
    println!(
        "{:<22} {:<10} {:<16} {:>10} {:>10} {:>10}  Dates",
        "Model", "Tier", "Request input", "Input", "Cached", "Output"
    );
    for rate in metadata["rates"].as_array().into_iter().flatten() {
        let value = |key: &str| rate[key].as_str().unwrap_or("n/a");
        let context = match (rate["min_input"].as_u64(), rate["max_input"].as_u64()) {
            (None, None) => "all".to_string(),
            (Some(a), Some(b)) => format!("{a}-{b}"),
            (Some(a), None) => format!(">={a}"),
            (None, Some(b)) => format!("<={b}"),
        };
        println!(
            "{:<22} {:<10} {:<16} {:>10} {:>10} {:>10}  {}–{}",
            value("model"),
            value("tier"),
            context,
            value("input"),
            value("cached"),
            value("output"),
            value("from"),
            value("until")
        );
    }
    Ok(())
}
fn load_pricer(cli: &Cli) -> Result<Pricer> {
    if let Some(date) = cli.pricing_date {
        Pricer::load(cli.pricing_file.as_deref(), Some(date))
    } else {
        Pricer::load_current(cli.pricing_file.as_deref())
    }
}
fn strict(cli: &Cli, report: &Report, allow_wait: bool) -> Result<()> {
    if !allow_wait && report.sources.iter().all(|s| !s.readable) {
        bail!("No selected source is readable; run codex-usage doctor");
    }
    if cli.strict {
        if report.coverage.incomplete() || report.sources.iter().any(|s| !s.readable) {
            bail!("incomplete local data; run doctor or omit --strict to inspect coverage");
        }
        if !cli.no_cost && (report.totals.unpriced_tokens > 0 || report.totals.assumed_events > 0) {
            bail!(
                "unpriced or assumed costs; inspect pricing, supply custom rates, or use --no-cost"
            );
        }
    }
    Ok(())
}
fn scan_report(
    cli: &Cli,
    tz: Tz,
    sources: &[Source],
    reader: &mut Reader,
    pricer: Option<&Pricer>,
    stopped: &AtomicBool,
) -> Result<(Scan, Report, u128)> {
    cancel::check(stopped)?;
    let started = Instant::now();
    let period = report::Period::make(cli, tz, Utc::now())?;
    let scan = if cli.monitor() {
        let cutoff = period
            .start
            .and_then(|start| start.checked_sub_signed(chrono::Duration::hours(12)));
        reader.scan_modified_since(sources, cutoff)?
    } else {
        reader.scan_since(sources, period.start)?
    };
    let report = report::build_cancellable(cli, tz, &scan, pricer, Utc::now(), stopped)?;
    Ok((scan, report, started.elapsed().as_millis()))
}
fn compact_pricing(report: &mut Report) {
    if let Some(metadata) = report.pricing.as_object_mut() {
        metadata.remove("rates");
    }
}
fn run(
    cli: &Cli,
    tz: Tz,
    sources: &[Source],
    pricer: Option<&Pricer>,
    format: Format,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    let mut reader = Reader::with_cancel(Arc::clone(&stopped));
    let loading = ui::interactive();
    if loading {
        eprint!("Reading sessions… Ctrl+C to cancel");
        io::stderr().flush()?;
    }
    let result = scan_report(cli, tz, sources, &mut reader, pricer, &stopped);
    if loading {
        eprint!("\r\x1b[2K");
        io::stderr().flush()?;
    }
    let (scan, mut report, millis) = result?;
    compact_pricing(&mut report);
    if matches!(cli.command, Some(Command::Doctor)) {
        if format == Format::Json {
            export::json(&report, io::stdout(), false)?;
        } else {
            for source in &report.sources {
                println!(
                    "{} {} / {}",
                    source.name,
                    if source.readable {
                        "ready"
                    } else {
                        "unreadable"
                    },
                    source.root.display()
                );
                if let Some(error) = &source.error {
                    println!("  {error}");
                }
            }
            println!(
                "{} files / {} usage records / {} duplicate records",
                report.coverage.files,
                scan.events.len(),
                report.coverage.duplicates
            );
            for message in &report.coverage.messages {
                println!("{message}");
            }
        }
        strict(cli, &report, false)?;
        return Ok(());
    }
    strict(cli, &report, cli.monitor())?;
    if cli.conversation() {
        let Some(Command::Session { id }) = &cli.command else {
            unreachable!()
        };
        let conversation = ui::load_conversation(cli, sources, id, &stopped, false)?;
        return if format == Format::Json {
            export::conversation_json(&conversation, &report, io::stdout())
        } else if ui::interactive() && cli.pager != codex_usage::cli::Pager::Never {
            conversation_ui::run(cli, conversation, &report, sources, stopped, true)?;
            Ok(())
        } else {
            let mut out = io::stdout().lock();
            write!(out, "{}", conversation_ui::plain(&conversation, tz))?;
            out.flush()?;
            Ok(())
        };
    }
    if !cli.monitor() {
        return match format {
            Format::Json => export::json(&report, io::stdout(), false),
            Format::Csv => export::csv(&report, io::stdout()),
            _ => {
                ui::run(cli, report, sources, stopped, || unreachable!())?;
                Ok(())
            }
        };
    }
    let mut tracker = Tracker::with_interval(&scan, &report, !cli.no_cost, cli.interval());
    tracker.seed_chart(cli, &scan, &report);
    report.monitor = Some(tracker.info(millis));
    let collector_stop = Arc::clone(&stopped);
    let mut refresh = || -> Result<Report> {
        let (scan, mut next, millis) =
            scan_report(cli, tz, sources, &mut reader, pricer, &collector_stop)?;
        compact_pricing(&mut next);
        strict(cli, &next, true)?;
        tracker.observe(cli, &scan, &mut next, millis);
        strict(cli, &next, true)?;
        Ok(next)
    };
    if format == Format::Table && ui::interactive() {
        ui::run(cli, report, sources, stopped, refresh)?;
        return Ok(());
    }
    let mut next_refresh = Instant::now() + cli.interval();
    loop {
        let final_frame = stopped.load(Ordering::Relaxed)
            || cli
                .frames()
                .is_some_and(|n| report.monitor.as_ref().is_some_and(|m| m.sequence + 1 >= n));
        if let Some(info) = &mut report.monitor {
            info.final_snapshot = final_frame;
        }
        if format == Format::Jsonl {
            export::json(&report, io::stdout(), true)?;
        } else {
            let mut out = io::stdout().lock();
            for line in render::linear(cli, &report, 120, false) {
                writeln!(out, "{line}")?;
            }
            out.flush()?;
        }
        if final_frame {
            break;
        }
        let deadline = next_refresh;
        while Instant::now() < deadline && !stopped.load(Ordering::Relaxed) {
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(std::time::Duration::from_millis(100)),
            );
        }
        if stopped.load(Ordering::Relaxed) {
            if let Some(info) = &mut report.monitor {
                info.final_snapshot = true;
                info.sequence += 1;
            }
            report.generated_at = Utc::now();
            if format == Format::Jsonl {
                export::json(&report, io::stdout(), true)?;
            }
            break;
        }
        next_refresh = Instant::now() + cli.interval();
        match refresh() {
            Ok(next) => report = next,
            Err(error) if cancel::is_cancelled(&error) => {
                // Keep the last complete snapshot; the next loop emits its final marker.
                if let Some(info) = &mut report.monitor {
                    info.sequence += 1;
                }
                report.generated_at = Utc::now();
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
