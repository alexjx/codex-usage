use clap::CommandFactory;
use codex_usage::cli::Cli;
use std::process::Command;

#[test]
fn every_command_and_argument_has_a_help_description() {
    fn check(command: &clap::Command) {
        assert!(
            command
                .get_about()
                .is_some_and(|text| !text.to_string().trim().is_empty()),
            "Missing command description: {}",
            command.get_name()
        );
        for arg in command.get_arguments() {
            assert!(
                arg.get_help()
                    .is_some_and(|text| !text.to_string().trim().is_empty()),
                "Missing argument description: {} / {}",
                command.get_name(),
                arg.get_id()
            );
        }
        for subcommand in command.get_subcommands().filter(|c| c.get_name() != "help") {
            check(subcommand);
        }
    }
    let mut command = Cli::command();
    command.build();
    check(&command);
}

fn help(path: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_codex-usage"))
        .args(path)
        .arg("--help")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{path:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn every_help_path_has_examples_and_action_specific_options() {
    for path in [
        vec![],
        vec!["models"],
        vec!["daily"],
        vec!["monthly"],
        vec!["sessions"],
        vec!["session"],
        vec!["monitor"],
        vec!["sources"],
        vec!["pricing"],
        vec!["doctor"],
        vec!["sources", "set"],
        vec!["sources", "enable"],
        vec!["sources", "disable"],
    ] {
        let text = help(&path);
        assert!(
            text.contains("Usage:") && text.contains("Example"),
            "{path:?}: {text}"
        );
    }
    let root = help(&[]);
    for description in [
        "last successful command",
        "this month",
        "including today",
        "Ctrl+C",
        "first run uses daily",
        "--reset",
    ] {
        assert!(root.contains(description), "{root}");
    }
    let monitor = help(&["monitor"]);
    for description in [
        "1000ms",
        "initial baseline",
        "jsonl when redirected",
        "60s per-session",
    ] {
        assert!(monitor.contains(description), "{monitor}");
    }
    for flag in ["--today", "--last", "--all", "--since", "--until"] {
        assert!(
            !monitor
                .lines()
                .any(|line| line.trim_start().starts_with(flag)),
            "{monitor}"
        );
    }
    for path in [
        vec!["sources"],
        vec!["sources", "set"],
        vec!["sources", "enable"],
        vec!["sources", "disable"],
        vec!["pricing"],
    ] {
        let text = help(&path);
        for flag in [
            "--model",
            "--today",
            "--group-by",
            "--pager",
            "--no-cost",
            "--reset",
        ] {
            assert!(
                !text.lines().any(|line| line.trim_start().starts_with(flag)),
                "{path:?}: {text}"
            );
        }
        assert!(text.contains("--config"), "{text}");
    }
    let set = help(&["sources", "set"]);
    assert!(
        set.contains("--root <PATH>") && set.contains("--kind") && set.contains("<NAME>"),
        "{set}"
    );
    assert!(help(&["pricing"]).contains("--pricing-file"));
}

#[test]
fn built_in_help_command_resolves_nested_action_help() {
    for args in [
        vec!["help", "monitor"],
        vec!["help", "sources", "set"],
        vec!["sources", "help", "enable"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_codex-usage"))
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("Usage:") && text.contains("Example"),
            "{text}"
        );
    }
}

#[test]
fn explicit_arguments_include_monitor_overrides_but_not_defaults() {
    let matches = Cli::command()
        .try_get_matches_from(["codex-usage", "monitor"])
        .unwrap();
    let explicit = Cli::explicit_arguments(&matches);
    assert!(!explicit.iter().any(|id| id == "interval" || id == "frames"));
    let matches = Cli::command()
        .try_get_matches_from([
            "codex-usage",
            "--reset",
            "monitor",
            "--interval",
            "5s",
            "--frames",
            "3",
            "--model",
            "test",
        ])
        .unwrap();
    let explicit = Cli::explicit_arguments(&matches);
    for field in ["reset", "interval", "frames", "models"] {
        assert!(
            explicit.iter().any(|id| id == field),
            "missing {field}: {explicit:?}"
        );
    }
    assert!(!explicit.iter().any(|id| id == "format" || id == "color"));
}
