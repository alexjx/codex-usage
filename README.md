# Codex Usage

A Rust CLI for reporting local Codex CLI usage across Linux, WSL, and Windows. See token usage and estimated cost by model, compare Fast and Regular modes, or watch today's activity in a live terminal monitor.

Reads local session logs. No API key or Codex login is required.

## Install

With Rust and native C/C++ build tools installed:

```sh
git clone https://github.com/alexjx/codex-usage.git
cd codex-usage
cargo install --path . --locked
```

Or build and run locally:

```sh
cargo build --release --locked
./target/release/codex-usage --help
```

On Windows PowerShell, the local executable is `.\target\release\codex-usage.exe`.

## Quick start

```sh
codex-usage models --today    # today's usage by model and mode
codex-usage daily --last 7    # daily totals for the past seven calendar days
codex-usage monitor          # live view of today's usage
codex-usage sources          # inspect available Windows/WSL/local sources
```

When no sources are configured, the tool uses `CODEX_HOME`, or your `.codex` directory when that variable is unset.

## Windows and WSL sources

Configure each side separately. Run `codex-usage sources` to see the configured sources and accessible candidates. A discovered Windows source starts disabled; enable it using its listed name:

```sh
codex-usage sources enable windows
```

For explicit paths, use the examples for the system where you run this tool.

**From WSL:**

```sh
codex-usage sources set wsl --kind wsl --root "$HOME/.codex"
codex-usage sources set windows --kind windows --root /mnt/c/Users/YOUR_USER/.codex
```

**From Windows PowerShell:**

```powershell
codex-usage sources set windows --kind windows --root "$env:USERPROFILE\.codex"
codex-usage sources set wsl --kind wsl --root '\\wsl.localhost\Ubuntu\home\YOUR_USER\.codex'
```

Replace the user names and WSL distribution name. Paths must be accessible from the system running the tool; the source kind does not translate paths or start a WSL distribution.

```sh
codex-usage sources disable windows
codex-usage sources enable windows
codex-usage daily --last 7 --source windows
codex-usage monitor --source wsl --source windows
```

Without `--source`, all enabled sources are included. Explicit `--source` selects those names for that invocation, even if disabled by default. New sources start enabled; updating a source preserves its enabled state. Identifiable copies of the same usage across sources are counted once.

For a one-off path, bypass configured sources with `--root`:

```sh
codex-usage models --root /path/to/.codex --today
```

## Reports

| Command | Shows |
| --- | --- |
| `models` | Usage by model and recorded service mode |
| `daily` | Dates with model/mode rows underneath |
| `monthly` | Usage by month, model, and mode |
| `sessions` | Each session's model/mode breakdown |
| `session SESSION_ID` | One session's usage |

Bare `codex-usage` repeats your last successful usage command and its options. With no saved command, it runs **daily for this calendar month**.

Repeating the same command reuses its saved options. Only options you explicitly supply replace their remembered values. Switching to a different command starts with defaults and replaces the remembered command; there is just one saved command, not a separate history for each command.

```sh
codex-usage daily --last 7 --mode fast
codex-usage daily                   # same daily report, last 7 days, Fast mode
codex-usage daily --last 1          # keep Fast mode, change the period to today
codex-usage                         # repeat the same daily report
codex-usage models                  # switch: model report with default options
codex-usage daily                   # switch again: daily defaults, not the old 7-day settings
codex-usage daily --reset           # reset this command's options to defaults
```

`--reset` clears remembered options before applying the options supplied on that run. Without a subcommand, it keeps the remembered command (or `daily` if none exists). For a report, `codex-usage --reset --last 7` keeps that command but resets other filters and display settings. Output format is remembered too: use `--format table` to override a previous JSON/CSV export.

Relative periods are recalculated each run. Monitor is also remembered, including its interval, frame limit, and filters; bare `codex-usage` repeats it after a successful monitor run. Source-management, pricing, and doctor commands do not replace the remembered usage command. Failed or cancelled loading does not save options.

### Read a session conversation

```sh
codex-usage sessions --last 7
codex-usage session SESSION_ID --conversation
codex-usage session SESSION_ID --conversation --format json > conversation.json
```

In the sessions viewer, select a row with arrows or `j`/`k` and press **Enter**. `Esc` returns to the same selected row; `q` quits. Short session reports also open the viewer; `--pager never` keeps normal linear output.

The reader shows user and assistant messages, with a turn outline on wide terminals. Tools and long output start collapsed. Use `Tab` to focus a block and Enter to expand it, `t` to show tools, `[` / `]` to jump turns, **m/M** to move between turns matching the selected model/mode, `/` to search text or model labels, `n` / `N` for matches, and `i` for session/source details. `r` reloads logs when they change. Ctrl+C cancels loading and restores the terminal.

Opening a model/mode row starts at a matching invocation, keeping nearby prompt context visible. Page keys use the actual visible page, and resizing preserves the position within a message. Conversations include all retained context for the selected session and sources, even when the usage report is date-filtered. Model focus guides navigation; the details panel retains the session usage estimate under the original report filters and period. Messages load only when opened; browsing does not change token/cost accounting or your remembered report options.

A direct `session ID --conversation` invocation is remembered like other commands. Piped output and `--pager never` produce the full conversation, including tool results; JSON wraps `conversation` and the filtered `usage` report. CSV is for usage reports and is rejected for conversations. Missing or encrypted text is shown as unavailable; attachments are labeled rather than opened.

### Dates and filters

| Option | Meaning |
| --- | --- |
| `--today` | Today in the selected timezone |
| `--last N` | N calendar days including today; `--last 1` means today |
| `--since DATE --until DATE` | Inclusive date bounds, in `YYYY-MM-DD` format |
| `--all` | All retained usage |
| `--timezone ZONE` | Date boundaries, for example `Asia/Shanghai`; defaults to config or host timezone |
| `--model ID` | Exact recorded model ID; repeat to include several |
| `--mode fast` | Recorded mode: `regular`, `fast`, `flex`, `ultrafast`, `batch`, or `unknown` |
| `--session ID` | Session ID; repeat to include several |
| `--project NAME_OR_PATH` | Recorded working directory or its final directory name |

Choose one date style: `--today`, `--last`, `--all`, or `--since`/`--until`.

```sh
codex-usage daily --since 2026-11-01 --until 2026-11-04
codex-usage monthly --all
codex-usage sessions --last 7 --project my-project
codex-usage session SESSION_ID --all
codex-usage models --today --mode fast
codex-usage models --all --group-by source,model
```

Daily reports show each date once. Every table ends with a **Total** for the full selection, including offscreen rows. Each date section ends with a **Subtotal** in the same columns; monthly sections get their own subtotal too. Session counts are unique within each period. JSON exposes these in a separate `subtotals` array; CSV keeps only data rows so aggregates are not counted twice. Long reports open a scrollable viewer with pinned headings and totals.

### Display and navigation

| Key | Action |
| --- | --- |
| Arrow keys or `j` / `k` | Scroll |
| Page Up / Page Down | Move a page |
| Home / End | Go to the first / last rows |
| `?` | Show help and accounting details |
| `c` | Toggle the monitor chart when space allows |
| `q` or Ctrl+C | Exit the viewer or monitor |

Ctrl+C also cancels file loading.

Use `--wide` for extra token breakdowns, `--exact` for full counts, and `--pager never` to print all rows into normal scrollback. Color adapts to the terminal; `--theme light`, `--theme dark`, `--color never`, and `--ascii` provide alternatives. `NO_COLOR` disables automatic color styling.

## Live monitor

```sh
codex-usage monitor
codex-usage monitor --interval 5s
codex-usage monitor --session SESSION_ID
```

Monitor always summarizes **today**, in the selected timezone. The screen places today's summary and live rates at the top, a speed chart below, then the model table. The full-selection total and controls stay at the bottom. History bars expand across the available width, and chart height grows with terminal space. The chart shrinks or hides on small terminals; scrolling does not pause updates.

| Indicator | What it measures |
| --- | --- |
| `Today 00:00:00–HH:MM:SS` | Tokens and estimated cost from local midnight through the latest snapshot, including loaded history |
| `2s: +… tokens / +… USD` | Net changes since the previous refresh; `2s` follows `--interval` |
| `… out tok/s` | Fresh output tokens divided by the actual time since the previous refresh |
| `(… tok/s)` | Mean output speed per active session over a **60-second** sliding window |
| Chart: `30s avg` | Combined output across sessions over a **30-second** sliding window, sampled each refresh |

The chart opens with up to **60 points** reconstructed from today's already-loaded records—about two minutes at the default 2s interval. Historical points use logged timestamps; live points use observed arrivals. Historical tokens carry into chart smoothing without becoming new-activity deltas. The chart header shows the average and peak of the displayed points; zeros count toward the average and unavailable gaps do not. Dots show zero; `?` marks unavailable data. Midnight resets totals and chart history.

Logs can arrive in batches, so speeds describe observed throughput rather than exact streaming generation speed. Near startup, the per-session average uses elapsed monitoring time until its window fills. The chart uses the available part of today near midnight. Recovered backlog and corrections can change token/cost deltas but are excluded from fresh-output speed. A complete source outage creates a chart gap and restarts smoothing on recovery.

The default refresh is `2s`; `--interval` accepts seconds or milliseconds from `1s` to `3600s`, such as `1500ms`. Slow scans extend the measured interval. Historical date options such as `--last` belong to reports and are rejected by monitor.

## Cost estimates and token counts

Costs are **API-equivalent token estimates in USD**, using current bundled prices for all session dates. They are not subscription charges, an account balance, or an invoice. Tool fees, taxes, and negotiated discounts are excluded. Inspect the bundled snapshot with:

```sh
codex-usage pricing
codex-usage models --all --no-cost
```

Input includes cached input; output includes reasoning. **Total = input + output**. Do not add the cached or reasoning columns again. Recorded Fast/priority and Regular/default modes appear separately; missing mode metadata remains Unknown.

Unknown prices show `n/a`. An asterisk (`*`) marks estimates with incomplete coverage or assumptions; `?` in the viewer and JSON exports provide details. Unpriced usage is not treated as free. `--strict` fails on incomplete data or, when pricing is enabled, missing prices and pricing assumptions. `--no-cost` skips pricing.

To override rates, create a TOML file. Values are decimal strings in USD per million tokens; this example uses fictional prices:

```toml
[[rates]]
model = "my-model"
provider = "openai"
tier = "standard"
input = "3"
cached = "1"
output = "10"
```

```sh
codex-usage models --all --pricing-file ./rates.toml
```

Matching custom entries override bundled rates; unmatched usage still uses the bundle.

## Export

```sh
codex-usage daily --all --format json > usage.json
codex-usage daily --all --format csv > usage.csv
codex-usage monitor --format jsonl --frames 5 > live.jsonl
```

Redirected monitoring defaults to JSONL. Each line is a **complete snapshot**, so do not sum snapshots together. `--frames N` stops after N snapshots, including startup. To print plain monitor snapshots instead, use `--format table`.

Exports contain exact counts and decimal cost values without terminal colors. JSONL includes chart history, per-session speeds, current-interval changes, and cumulative changes since startup. CSV contains report rows. Diagnostics go to stderr.

## Configuration and troubleshooting

On Linux/WSL, configuration normally lives at `~/.config/codex-usage/config.toml` (subject to XDG settings). Use `--config PATH` or `CODEX_USAGE_CONFIG` for another location on any platform. The last successful usage command and its options are saved separately beside that file as `<config filename>.last-report.json`. Each configuration profile has its own saved command. The file stores options, not session data. Existing saved report settings are reused.

```sh
codex-usage sources
codex-usage doctor --no-cost
codex-usage doctor --source windows --format json
codex-usage doctor --all --strict --no-cost
codex-usage monitor --help
```

- **A source is disabled:** enable its exact listed name, or select it explicitly with `--source`.
- **A Windows/WSL path is unavailable:** check the path from the system running this tool and make sure the WSL distribution is accessible.
- **A price is missing:** inspect `pricing`, provide an override, or use `--no-cost`.
- **Data is partial:** inspect `doctor` and source status. Deleted logs, incomplete writes, unsupported records, and unreadable sources can limit coverage.
- **Imported logs seem missing:** date-limited reports filter file modification times before parsing. Reports allow a 48-hour buffer before the selected start; monitor allows 12 hours before today's midnight. Old sessions updated today remain eligible. Use an all-time report to inspect imported logs whose old modification times fall outside the filter.

Session files are read-only. Reports reconstruct retained local data rather than querying your OpenAI account.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --locked
```

Tests use temporary synthetic session logs; one optional local-log smoke test is ignored by default. Linux and native Windows test suites have been run; native Windows console appearance has not been separately checked.

## License

[MIT](LICENSE).
