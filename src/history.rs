//! Last successful usage command and its options, isolated by config profile.
use crate::{
    cli::{Cli, Command},
    config::normalize_path,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedReport {
    schema_version: u32,
    report: Cli,
}

/// A profile never reuses the remembered report from a different config file.
pub fn state_path(config_path: &Path) -> PathBuf {
    let mut name = config_path.file_name().unwrap_or_default().to_os_string();
    name.push(".last-report.json");
    config_path.with_file_name(name)
}

fn is_usage(cli: &Cli) -> bool {
    matches!(
        cli.command,
        None | Some(
            Command::Models
                | Command::Daily
                | Command::Monthly
                | Command::Sessions
                | Command::Session { .. }
                | Command::Monitor { .. }
        )
    )
}

fn validate_report(cli: &Cli) -> Result<()> {
    if !is_usage(cli) {
        bail!("Remembered command is not a usage command");
    }
    if cli.has_monitor_range() {
        bail!("Remembered monitor has historical date options");
    }
    if let Some(Command::Monitor { interval, frames }) = &cli.command {
        if *interval < std::time::Duration::from_secs(1)
            || *interval > std::time::Duration::from_secs(3600)
            || *frames == Some(0)
        {
            bail!("Remembered monitor has an invalid interval or frame limit");
        }
    }
    if !cli.sources.is_empty() && !cli.roots.is_empty() {
        bail!("Remembered report selects both named sources and ad hoc roots");
    }
    let relative_period =
        usize::from(cli.today) + usize::from(cli.all) + usize::from(cli.last.is_some());
    if relative_period > 1
        || (relative_period > 0 && (cli.since.is_some() || cli.until.is_some()))
        || cli.last == Some(0)
        || cli
            .since
            .zip(cli.until)
            .is_some_and(|(start, end)| start > end)
    {
        bail!("Remembered report has conflicting or invalid date options");
    }
    if cli.roots.iter().any(|root| !root.is_absolute())
        || cli
            .pricing_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
    {
        bail!("Remembered report contains relative paths");
    }
    Ok(())
}

fn state_error(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(
        "Cannot reuse remembered report {}: {error}. Run an explicit report command (for example `codex-usage models`) to replace it, or remove that state file",
        path.display()
    )
}

fn load(path: &Path) -> Result<Option<Cli>> {
    let input = match fs::read(path) {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_error(path, error)),
    };
    let saved: SavedReport =
        serde_json::from_slice(&input).map_err(|error| state_error(path, error))?;
    if saved.schema_version != SCHEMA_VERSION {
        return Err(state_error(
            path,
            format!("unsupported schema version {}", saved.schema_version),
        ));
    }
    validate_report(&saved.report).map_err(|error| state_error(path, error))?;
    Ok(Some(saved.report))
}

fn defaults(mut cli: Cli) -> Cli {
    cli.command.get_or_insert(Command::Daily);
    cli
}

/// Reuse options for the same usage command; switching commands starts fresh.
/// Bare invocations choose the saved command, or Daily if history is absent.
pub fn resolve(mut cli: Cli, config_path: &Path) -> Result<Cli> {
    if !is_usage(&cli) {
        return Ok(cli);
    }
    let saved = match load(&state_path(config_path)) {
        Ok(saved) => saved,
        // An explicit command or reset can recover from unusable preferences.
        Err(_) if cli.command.is_some() || cli.reset => None,
        Err(error) => return Err(error),
    };
    let Some(mut report) = saved else {
        return Ok(defaults(cli));
    };
    let saved_command = report.command.clone().unwrap_or(Command::Daily);
    let command = cli.command.clone().unwrap_or_else(|| saved_command.clone());
    if cli.reset || std::mem::discriminant(&command) != std::mem::discriminant(&saved_command) {
        cli.command = Some(if cli.reset && cli.command.is_none() {
            match command {
                Command::Monitor { .. } => Command::Monitor {
                    interval: std::time::Duration::from_secs(2),
                    frames: None,
                },
                command => command,
            }
        } else {
            command
        });
        return Ok(cli);
    }
    report.command = Some(match (&command, &saved_command) {
        (
            Command::Monitor { interval, frames },
            Command::Monitor {
                interval: previous_interval,
                frames: previous_frames,
            },
        ) => Command::Monitor {
            interval: if cli.explicit.iter().any(|name| name == "interval") {
                *interval
            } else {
                *previous_interval
            },
            frames: if cli.explicit.iter().any(|name| name == "frames") {
                *frames
            } else {
                *previous_frames
            },
        },
        _ => command,
    });
    let supplied = |field: &str| cli.explicit.iter().any(|name| name == field);

    // A snapshot can never move a bare invocation to a different config profile.
    report.config.clone_from(&cli.config);
    report.explicit.clone_from(&cli.explicit);
    if supplied("sources") {
        report.sources.clone_from(&cli.sources);
        report.roots.clear();
    }
    if supplied("roots") {
        report.roots.clone_from(&cli.roots);
        report.sources.clear();
    }
    if ["today", "all", "since", "until", "last"]
        .iter()
        .any(|field| supplied(field))
    {
        report.today = cli.today;
        report.all = cli.all;
        report.since = cli.since;
        report.until = cli.until;
        report.last = cli.last;
    }
    macro_rules! replace {
        ($($field:ident),+ $(,)?) => {
            $(if supplied(stringify!($field)) { report.$field.clone_from(&cli.$field); })+
        };
    }
    replace!(
        models,
        mode,
        sessions,
        projects,
        timezone,
        group_by,
        sort,
        format,
        color,
        theme,
        pager,
        ascii,
        exact,
        wide,
        no_cost,
        strict,
        pricing_file,
        pricing_date,
        conversation
    );
    // Explicit relative paths are valid at invocation time; normalize them before validation.
    // All paths retained from the saved record were already required to be absolute above.
    if supplied("roots") {
        report.roots = report
            .roots
            .iter()
            .map(|path| normalize_path(path))
            .collect::<Result<_>>()?;
    }
    if supplied("pricing_file") {
        report.pricing_file = report
            .pricing_file
            .as_deref()
            .map(normalize_path)
            .transpose()?;
    }
    validate_report(&report)?;
    Ok(report)
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Save only successful usage commands; administration leaves history untouched.
pub fn save(cli: &Cli, config_path: &Path) -> Result<()> {
    if !is_usage(cli) {
        return Ok(());
    }
    let mut report = cli.clone();
    if report.command.is_none() {
        report.command = Some(Command::Daily);
    }
    report.explicit.clear();
    report.roots = report
        .roots
        .iter()
        .map(|path| normalize_path(path))
        .collect::<Result<_>>()?;
    report.pricing_file = report
        .pricing_file
        .as_deref()
        .map(normalize_path)
        .transpose()?;
    validate_report(&report)?;
    let state = SavedReport {
        schema_version: SCHEMA_VERSION,
        report,
    };
    let bytes = serde_json::to_vec_pretty(&state).context("Cannot encode remembered report")?;
    let path = normalize_path(&state_path(config_path))?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Remembered report path has no parent"))?;
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut temporary = None;
    for _ in 0..100 {
        let path = parent.join(format!(
            ".last-report-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                temporary = Some((Temporary(path), file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("Cannot reserve remembered-report file"),
        }
    }
    let (temporary, mut file) =
        temporary.ok_or_else(|| anyhow!("Cannot reserve remembered-report file"))?;
    file.write_all(&bytes)
        .context("Cannot write remembered report")?;
    file.sync_all().context("Cannot flush remembered report")?;
    drop(file);
    fs::rename(&temporary.0, &path)
        .with_context(|| format!("Cannot replace remembered report {}", path.display()))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Color, Format, Mode, Pager, Sort, Theme};
    use clap::{CommandFactory, FromArgMatches};
    use serde_json::json;
    use tempfile::tempdir;

    fn cli(args: &[&str]) -> Cli {
        let matches = Cli::command()
            .try_get_matches_from(std::iter::once("codex-usage").chain(args.iter().copied()))
            .unwrap();
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        cli.explicit = Cli::explicit_arguments(&matches);
        cli
    }

    #[test]
    fn bare_invocation_replays_report_and_explicit_globals_replace_saved_options() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let previous = cli(&[
            "daily",
            "--last",
            "14",
            "--model",
            "model-a",
            "--mode",
            "fast",
            "--timezone",
            "Asia/Shanghai",
            "--format",
            "csv",
            "--color",
            "always",
            "--theme",
            "dark",
            "--pager",
            "never",
            "--sort",
            "output",
            "--exact",
            "--wide",
            "--ascii",
            "--no-cost",
            "--strict",
        ]);
        save(&previous, &path).unwrap();
        let replay = resolve(cli(&[]), &path).unwrap();
        assert!(matches!(replay.command, Some(Command::Daily)));
        assert_eq!(replay.last, Some(14));
        assert_eq!(replay.models, ["model-a"]);
        assert_eq!(replay.mode, Some(Mode::Fast));
        assert_eq!(replay.format, Some(Format::Csv));
        assert_eq!(replay.sort, Some(Sort::Output));
        assert_eq!(replay.color, Color::Always);
        assert_eq!(replay.theme, Theme::Dark);
        assert_eq!(replay.pager, Pager::Never);
        assert!(replay.exact && replay.wide && replay.ascii && replay.no_cost && replay.strict);
        assert_eq!(replay.timezone.as_deref(), Some("Asia/Shanghai"));
        assert!(replay.explicit.is_empty());
        let overridden = resolve(
            cli(&[
                "--last", "3", "--model", "model-b", "--mode", "regular", "--format", "json",
                "--color", "never", "--theme", "light", "--pager", "auto", "--sort", "total",
            ]),
            &path,
        )
        .unwrap();
        assert!(matches!(overridden.command, Some(Command::Daily)));
        assert_eq!(overridden.last, Some(3));
        assert_eq!(overridden.models, ["model-b"]);
        assert_eq!(overridden.mode, Some(Mode::Regular));
        assert_eq!(overridden.format, Some(Format::Json));
        assert_eq!(overridden.color, Color::Never);
        assert_eq!(overridden.theme, Theme::Light);
        assert_eq!(overridden.pager, Pager::Auto);
        assert_eq!(overridden.sort, Some(Sort::Total));
        assert_eq!(overridden.timezone.as_deref(), Some("Asia/Shanghai"));
    }

    #[test]
    fn period_overrides_clear_saved_period_conflicts_and_source_overrides_clear_other_selector() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&cli(&["monthly", "--all", "--source", "windows"]), &path).unwrap();
        let root = dir.path().join("new-root");
        let overridden = resolve(
            cli(&[
                "--root",
                root.to_str().unwrap(),
                "--since",
                "2026-10-01",
                "--until",
                "2026-10-03",
            ]),
            &path,
        )
        .unwrap();
        assert!(!overridden.all && !overridden.today && overridden.last.is_none());
        assert_eq!(overridden.since.unwrap().to_string(), "2026-10-01");
        assert_eq!(overridden.until.unwrap().to_string(), "2026-10-03");
        assert!(overridden.sources.is_empty());
        assert_eq!(overridden.roots, [normalize_path(&root).unwrap()]);
        save(&overridden, &path).unwrap();
        let named = resolve(cli(&["--source", "wsl", "--today"]), &path).unwrap();
        assert!(named.today && !named.all && named.since.is_none() && named.until.is_none());
        assert_eq!(named.sources, ["wsl"]);
        assert!(named.roots.is_empty());
        let last = resolve(cli(&["--last", "7"]), &path).unwrap();
        assert_eq!(last.last, Some(7));
        assert!(!last.today && !last.all && last.since.is_none() && last.until.is_none());
    }

    #[test]
    fn switching_commands_starts_fresh_and_administration_never_replaces_history() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&cli(&["daily", "--last", "7", "--no-cost"]), &path).unwrap();
        let original = fs::read(state_path(&path)).unwrap();
        for args in [
            vec!["sources"],
            vec!["sources", "enable", "wsl"],
            vec!["pricing"],
            vec!["doctor"],
        ] {
            let fresh = resolve(cli(&args), &path).unwrap();
            assert!(fresh.last.is_none());
            save(&fresh, &path).unwrap();
            assert_eq!(fs::read(state_path(&path)).unwrap(), original);
        }
        let fresh = resolve(cli(&["models"]), &path).unwrap();
        assert!(matches!(fresh.command, Some(Command::Models)));
        assert!(!fresh.no_cost && fresh.last.is_none());
    }

    #[test]
    fn monitor_overrides_and_bare_reset_do_not_reuse_parser_defaults_as_explicit_values() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(
            &cli(&[
                "monitor",
                "--interval",
                "5s",
                "--frames",
                "3",
                "--format",
                "jsonl",
                "--wide",
            ]),
            &path,
        )
        .unwrap();
        for args in [vec![], vec!["monitor"]] {
            let replay = resolve(cli(&args), &path).unwrap();
            assert_eq!(replay.interval(), std::time::Duration::from_secs(5));
            assert_eq!(replay.frames(), Some(3));
            assert!(replay.wide);
        }
        let overridden = resolve(
            cli(&["monitor", "--interval", "2s", "--frames", "1"]),
            &path,
        )
        .unwrap();
        assert_eq!(overridden.interval(), std::time::Duration::from_secs(2));
        assert_eq!(overridden.frames(), Some(1));
        let reset = resolve(cli(&["--reset"]), &path).unwrap();
        assert!(reset.monitor());
        assert_eq!(reset.interval(), std::time::Duration::from_secs(2));
        assert_eq!(reset.frames(), None);
        assert_eq!(reset.format, None);
        assert!(!reset.wide);
        save(&reset, &path).unwrap();
        let encoded = fs::read_to_string(state_path(&path)).unwrap();
        assert!(!encoded.contains("\"reset\""));
        assert!(!resolve(cli(&[]), &path).unwrap().reset);
    }

    #[test]
    fn profiles_are_isolated_and_current_config_is_never_replaced_by_snapshot() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("work.toml");
        let second = dir.path().join("personal.toml");
        save(&cli(&["--config", "old-config.toml", "sessions"]), &first).unwrap();
        let replay = resolve(cli(&["--config", "current-config.toml"]), &first).unwrap();
        assert!(matches!(replay.command, Some(Command::Sessions)));
        assert_eq!(replay.config, Some(PathBuf::from("current-config.toml")));
        let unset = resolve(cli(&[]), &first).unwrap();
        assert!(unset.config.is_none());
        let isolated = resolve(cli(&[]), &second).unwrap();
        assert!(matches!(isolated.command, Some(Command::Daily)));
        assert_eq!(
            state_path(&first),
            dir.path().join("work.toml.last-report.json")
        );
    }

    #[test]
    fn corrupt_unknown_version_and_nonreport_history_are_actionable_not_silently_replayed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(state_path(&path), "not json").unwrap();
        let error = resolve(cli(&[]), &path).unwrap_err().to_string();
        assert!(error.contains("explicit report command") && error.contains(".last-report.json"));
        assert!(resolve(cli(&["models"]), &path).is_ok());
        save(&cli(&["daily"]), &path).unwrap();
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(state_path(&path)).unwrap()).unwrap();
        state["schema_version"] = json!(2);
        fs::write(state_path(&path), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(resolve(cli(&[]), &path)
            .unwrap_err()
            .to_string()
            .contains("unsupported schema version"));
        let state = SavedReport {
            schema_version: 1,
            report: cli(&["pricing"]),
        };
        fs::write(state_path(&path), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(resolve(cli(&[]), &path)
            .unwrap_err()
            .to_string()
            .contains("not a usage command"));
    }

    #[test]
    fn save_normalizes_paths_replaces_atomically_and_initial_default_is_daily() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let mut report = cli(&["--root", "relative-root", "--pricing-file", "rates.toml"]);
        report.explicit = vec!["roots".into(), "pricing_file".into()];
        save(&report, &path).unwrap();
        let replay = resolve(cli(&[]), &path).unwrap();
        assert!(matches!(replay.command, Some(Command::Daily)));
        assert_eq!(
            replay.roots,
            [normalize_path(Path::new("relative-root")).unwrap()]
        );
        assert_eq!(
            replay.pricing_file,
            Some(normalize_path(Path::new("rates.toml")).unwrap())
        );
        assert!(replay.roots[0].is_absolute());
        let encoded = fs::read_to_string(state_path(&path)).unwrap();
        assert!(!encoded.contains("\"explicit\""));
        save(&cli(&["session", "thread-123"]), &path).unwrap();
        assert!(
            matches!(resolve(cli(&[]), &path).unwrap().command, Some(Command::Session { id }) if id == "thread-123")
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state_path(&path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
