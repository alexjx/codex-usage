# Codex Usage

Local Codex CLI usage reports and live monitoring, written in Rust. Read Windows and WSL session logs independently or together, compare models, and estimate API-equivalent token costs.

## Install

From this source directory, in WSL/Linux or native Windows PowerShell:

```sh
cargo install --path .
```

For a local build instead:

```sh
cargo build --release
```

The executable is `target/release/codex-usage` on Linux and `target\release\codex-usage.exe` on Windows. No Codex login or API key is needed. The tool reads local session logs; it does not query an account balance or remaining subscription quota.

## Configure Windows and WSL separately

If `sources` lists a discovered Windows folder, enable it by its listed name:

```sh
codex-usage sources
codex-usage sources enable windows
```

Enabling registers that folder and preserves the existing local source. With multiple profiles, use the exact listed name. Listing alone never enables a discovered folder. Use `sources set` below when you need a custom path.

When running in WSL:

```sh
codex-usage sources set wsl --kind wsl --root "$HOME/.codex"
codex-usage sources set windows --kind windows --root /mnt/c/Users/xinj/.codex
codex-usage sources
```

Replace the Windows profile path with your own. For nonstandard mounts, supply the accessible path explicitly.

When running in native Windows PowerShell:

```powershell
codex-usage sources set windows --kind windows --root "$env:USERPROFILE\.codex"
codex-usage sources set wsl --kind wsl --root '\\wsl.localhost\Ubuntu\home\xinj\.codex'
```

Replace the distribution and Linux user name. Paths must be accessible from the host running the tool; source kinds do not translate paths or start WSL distributions.

```sh
codex-usage models --source wsl
codex-usage models --source windows
codex-usage monitor --source wsl --source windows
codex-usage sources disable windows
codex-usage sources enable windows
```

Without `--source`, use all enabled sources. Explicit `--source` selects exactly those names for one invocation, including a source disabled for default reports. Updating one source preserves the other sources and its enabled state. Disabling every source produces a configuration error rather than silently falling back to local logs.

Before configuration, the native source defaults to `CODEX_HOME` or the user's `.codex` directory. An ad hoc root bypasses saved source selection:

```sh
codex-usage --root /path/to/.codex models --all
```

Configuration uses the native application configuration directory (`~/.config/codex-usage/config.toml` on Linux unless XDG settings override it). Use `--config /path/to/config.toml` or `CODEX_USAGE_CONFIG` to select another file. Configuration commands persist source settings; usage reports leave those settings unchanged and save their last-used report preferences separately. Diagnose source problems with `codex-usage doctor`.

## Reports

```sh
codex-usage
codex-usage daily --last 7
codex-usage daily --since 2026-11-01 --until 2026-11-04
codex-usage monthly --all
codex-usage sessions --today
codex-usage session <session-id> --all
codex-usage models --group-by source,model --all
codex-usage daily --project my-project --timezone Asia/Shanghai
codex-usage models --model gpt-6.1-sol --no-cost
```

Fresh report commands default to the current calendar month. Bare `codex-usage` repeats the last successful usage report and its options, or starts with the model report if none has been saved. Monitor always summarizes today. Model filters match exact recorded identifiers. Project filters match a recorded working directory or its final directory name.

`--last N` selects N calendar days including today in the selected timezone; `--last 7` starts six days ago, rather than selecting a rolling 168-hour window. `--last 1` is today. Date-only `--since`/`--until` bounds include both named dates. Choose one period style: `--last`, `--today`, `--all`, or `--since`/`--until`. Zero days and conflicting options are rejected. Monitor rejects `--last`, `--all`, `--since`, and `--until`; use a report command for those ranges.

Each model and recorded mode has a separate row. Fast/priority is shown as `Fast`, standard/default as `Regular`, and missing settings as `Unknown`. Other recorded modes retain their labels. Use `--mode fast`, `--mode regular`, or `--mode unknown` to filter; `--group-by model` deliberately combines modes and shows `Mixed` when needed. Unknown-mode cost can still use an explicitly disclosed Regular-rate assumption; it does not change the recorded mode. Input includes cached input; output includes reasoning. Total tokens are input plus output, not the sum of every breakdown column. Distinct session totals can be smaller than the sum of per-model session counts because a session can switch models. Missing breakdowns and unknown models are preserved rather than silently guessed.

Daily reports show each date once as a heading (for example, `Wed, Nov 04 2026`), with model/mode rows underneath. The current date stays visible when scrolling inside a date section. JSON and CSV retain the full ISO date on every row. Every table report ends with a bold Total row for the full selection, aligned to its displayed columns. In the report viewer and live monitor, this total stays pinned below the data while you scroll. Partial cost estimates retain their marker. Long reports open a scrollable terminal view with pinned headings and full-period totals. Use arrows or `j`/`k`, Page Up/Down, and Home/End; `q` or Ctrl+C exits. `?` reveals controls and accounting details. `--pager never` prints every row into normal scrollback. Redirected output never opens the viewer.

Color follows your terminal by default. Use `--theme light`, `--theme dark`, `--color never`, or `--ascii` as needed. `NO_COLOR` disables automatic styling. `--wide` exposes additional breakdown columns; `--exact` shows full counts. Unknown prices remain `n/a`; diagnostic counts are available through `?`, `doctor`, and JSON exports rather than normal footer lines.

## Remember the last report

```sh
codex-usage daily --last 7 --source windows --model gpt-6.1-sol
codex-usage                         # repeat the daily report with the same options
codex-usage --last 1                # same report and filters, but today only
codex-usage models                  # start a fresh model report for this month
```

The remembered options include period, sources or ad hoc roots, filters, grouping, sorting, timezone, pricing, and output/display settings. Explicit flags on a bare invocation replace the corresponding remembered fields; for example, `--model` replaces the remembered model filter. A new period flag replaces the previous period selection. Relative periods such as `--last 7` are recalculated when the command runs again. Supplying an explicit report command starts fresh rather than inheriting the previous command's filters or date range.

Preferences are saved only after a usage report succeeds, beside the selected configuration file as `<config filename>.last-report.json`. Separate `--config` profiles have separate report preferences. Monitor, source setup, doctor, pricing, and failed reports do not replace them. Only report options are remembered; each run reads usage data again.

## Live monitoring

```sh
codex-usage monitor
codex-usage monitor --interval 5s --source windows
codex-usage monitor --session <session-id>
```

Monitor always summarizes today in the selected timezone and resets its totals, deltas, and speed samples at local midnight. Historical range flags are not supported. Monitor updates one screen in place. Scrolling does not pause collection, and totals include offscreen rows. Quit with `q` or Ctrl+C; press `?` for details.

`out tok/s` is newly observed output tokens over a rolling five-second window, using actual elapsed time. It excludes startup history; log records can arrive in bursts, so this measures observed log throughput rather than the model's streaming generation speed. Tokens and costs update when Codex writes usage records.

The parenthesized rate, for example `120 out tok/s (40.0 tok/s)`, is the average per active session. It smooths output over a rolling 30-second window and averages the rates of sessions with fresh output in that window. Idle sessions drop out; startup uses the elapsed time until 30 seconds have passed. Startup history, recovered backlog, and corrections are excluded from both indicators. JSONL also includes individual rates in `monitor.session_output_tokens_per_second`, the mean in `average_session_output_tokens_per_second`, and `session_speed_window_seconds`. With `--session <id>`, the average is that session's rate.

## Cost estimates

Costs are API-equivalent model-token estimates in USD, not subscription charges or an invoice. Tool fees, regional uplifts, taxes, and negotiated discounts are outside their scope. Reasoning is included in output charges.

All usage is estimated at the current bundled prices, regardless of when the session ran. The bundled table is an official pricing snapshot verified on **2026-11-04**; its date and source are available through `pricing` and detailed exports. Prices are kept fixed during monitoring. This is a current-price estimate, not a reconstruction of historical charges.

```sh
codex-usage models --all
codex-usage pricing
```

Unknown models/providers, unsupported service tiers, and unverified cache-write billing semantics remain unpriced. Missing tier or cache metadata may require explicit assumptions, available in detailed output. Partial estimates are labeled as subtotals; unpriced usage is never treated as free. `--strict` fails on incomplete coverage or pricing assumptions when costs are enabled. `--no-cost` skips pricing.

Custom rates use decimal strings in USD per million tokens:

```toml
# rates.toml — fictional example, not a published model price
[[rates]]
model = "my-model"
provider = "openai"
tier = "standard"
input = "3"
cached = "1"
output = "10"
```

```sh
codex-usage models --pricing-file ./rates.toml --all
```

Matching custom entries override bundled rates; other requests use the bundle. Optional custom date limits are evaluated at the current pricing snapshot date, not the session date; omit them for ordinary overrides. Date bounds are inclusive. Optional `min_input`/`max_input` restrict request sizes. Verified cache-write rates can use `cache_write`, `cache_write_12h`, and an explicit `cache_write_mode` of `subset` or `additive`. Ambiguous overlapping entries and negative rates are rejected.

## Export and scripting

```sh
codex-usage daily --all --format json > usage.json
codex-usage daily --all --format csv > usage.csv
codex-usage monitor --format jsonl --frames 5 > live.jsonl
```

Monitor defaults to JSONL when redirected; `--format table` explicitly selects linear text snapshots. JSONL records are complete snapshots, not additive events. JSON/CSV/JSONL contain exact counts and decimal cost strings, without terminal escape codes. `--frames N` stops monitoring after a finite number of snapshots. Diagnostics use stderr.

Reports reconstruct retained local logs. Deleted logs, incomplete records, inherited histories with insufficient identity, or unavailable sources can limit coverage. Source status and detailed exports make these limitations inspectable. Copying logs between Windows and WSL does not make the copied usage independent; identifiable duplicates are counted once.

The original UI demonstration remains under `examples/ui-preview/`; it uses fictional data. The production executable is `codex-usage`.

## Data handling and performance

The first invocation scans retained session history, including compressed archives, to establish counters and inherited-history baselines before date filtering. Large Windows archives can make this initial scan slow. Monitor retains parsed state in memory and tails growing plain logs; it does not rescan all file contents on every refresh. There is no persistent usage cache.

Size, modification-time, and file-identity changes are checked each refresh. A progressive background content sweep begins 60 seconds after startup scanning or the preceding sweep completes; its completion time depends on the dataset. This catches rewrites that deliberately preserve size and modification time without blocking the live screen on an entire archive scan.

Older fork logs can replay parent history with rewritten timestamps. The reader separates identified current requests from legacy compatibility counters and uses recorded thread/turn identity to exclude inherited prefixes. Unsupported standalone compaction records and ambiguous counter resets are diagnosed rather than guessed. An unreadable file retains its previous known contributions where available and does not discard readable neighbors; an inaccessible directory can mark the whole selected source stale.

Validation uses synthetic unit/integration fixtures plus read-only local schema checks. Terminal tests exercise paging, resizing, real fixture appends, speed, and cleanup after normal exit and a refresh error. Neither tests nor reports require credentials, and session files are never modified.

Checked on Linux with the Rust test suite, formatter and Clippy, plus interactive PTY tests. Native Windows MSVC compilation, unit tests, CLI integrations and executable smoke tests also passed. Interactive Windows Terminal appearance has not been checked separately.
