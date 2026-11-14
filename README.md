# Agent Usage

A Rust CLI for reporting local Codex CLI and Claude Code usage across Linux, WSL, and Windows. See token usage and estimated API-equivalent cost by agent, provider, model, and service mode, or watch each agent's activity independently in a live terminal monitor.

Reads local session logs. No API key, login, or network request is required.

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
./target/release/agent-usage --help
```

On Windows PowerShell, the local executable is `.\target\release\agent-usage.exe`.

## Quick start

```sh
agent-usage models --today                 # today's usage by agent, provider, model, and mode
agent-usage daily --last 7                 # daily totals for the past seven calendar days
agent-usage monitor                        # live view with independent Codex and Claude activity
agent-usage sources                        # inspect configured and discoverable sources
agent-usage daily --agent claude-code      # Claude Code only
```

When no sources are configured, both native roots are discovered: `CODEX_HOME` or `~/.codex`, and `CLAUDE_CONFIG_DIR` or `~/.claude`. An explicit environment root is retained even when it is temporarily unreadable; a missing conventional directory is skipped.

## Agents and sources

Each source declares one agent (`codex` or `claude-code`) and a data root. The two agents share reporting, pricing, monitoring, exports, and conversation views, but never share native identities. The same raw session or message ID under both agents is two distinct records.

Configuration uses version 2:

```toml
version = 2
timezone = "Asia/Shanghai"

[[sources]]
name = "codex-linux"
agent = "codex"
kind = "linux"
root = "/home/example/.codex"
enabled = true

[[sources]]
name = "claude-linux"
agent = "claude-code"
kind = "linux"
root = "/home/example/.claude"
enabled = true
# pricing_provider = "anthropic" # only when the transcript cannot identify the backend
```

Unknown config fields are errors. Relative roots resolve against the config file. Source names are unique; the same directory is allowed once per agent.

Run `agent-usage sources` for the configured list and accessible candidates. A discovered Windows source starts disabled; enable it by its listed name:

```sh
agent-usage sources enable windows-USER-codex
agent-usage sources enable windows-USER-claude-code
```

For explicit paths, use the examples for the system where you run this tool.

**From WSL:**

```sh
agent-usage sources set codex --agent codex --kind wsl --root "$HOME/.codex"
agent-usage sources set claude --agent claude-code --kind wsl --root "$HOME/.claude"
agent-usage sources set windows-codex --agent codex --kind windows --root /mnt/c/Users/YOUR_USER/.codex
```

**From Windows PowerShell:**

```powershell
agent-usage sources set codex --agent codex --kind windows --root "$env:USERPROFILE\.codex"
agent-usage sources set claude --agent claude-code --kind windows --root "$env:USERPROFILE\.claude"
```

Replace the user names and WSL distribution name. Paths must be accessible from the system running the tool; the source kind does not translate paths or start a WSL distribution.

```sh
agent-usage sources disable windows-codex
agent-usage sources enable windows-codex
agent-usage daily --last 7 --source windows-codex
agent-usage monitor --source codex --source claude
```

Without `--source`, all enabled sources are included. Explicit `--source` includes those names even if disabled by default. New sources start enabled; updating a source preserves its enabled state. Identifiable copies of the same usage across mirrored roots are counted once.

For a one-off path, bypass configured sources with `--root`. An explicit `--agent` is required because the transcript layout is not inferred from the directory name:

```sh
agent-usage models --agent codex --root /path/to/.codex --today
agent-usage models --agent claude-code --root /path/to/.claude --all
```

`--agent` is repeatable and filters every usage command. On `sources set` it selects exactly one agent; on `enable`/`disable` an incompatible supplied agent is an error.

## Reports

Terminal reports group models under **Codex** and **Claude** headings, without agent or provider prefixes on model names. Daily and monthly reports nest these groups within each date or month and retain one subtotal per period. Provider metadata remains available in structured exports.

| Command | Shows |
| --- | --- |
| `models` | Usage by agent, provider, model, and recorded service mode |
| `daily` | Dates with agent/provider/model/mode rows underneath |
| `monthly` | Usage by month, agent, provider, model, and mode |
| `sessions` | Each session's agent/provider/model/mode breakdown |
| `session SELECTOR` | One session's usage |

Bare `agent-usage` and `daily` report **since the most recent Codex quota reset** when every selected source is Codex. `~` marks an estimated reset. Without usable reset data—or when selected sources disagree—they fall back to the last 7 days, including today.

Any Claude Code source changes the default to the **last seven calendar days**, because Claude quota retrieval is unsupported. A mixed selection reports the basis `last_7_days_multi_agent`; Claude-only reports `last_7_days_no_quota_capability`. A Claude source with no events still prevents a Codex reset from shortening the combined range.

```sh
agent-usage daily --last 7 --mode fast
agent-usage daily --agent claude-code --last 7
agent-usage models --group-by agent,provider,model --all
agent-usage                           # same default daily period
```

`--last N` selects a date window for that run. Source paths, enabled sources, and configured timezone remain persistent settings.

### Sessions and conversations

The session dimension is a qualified selector:

```text
<agent>:<percent-encoded-session>[:<percent-encoded-execution>]
codex:session-123
claude-code:session-123
claude-code:session-123:agent-456
```

Only `A-Z a-z 0-9 _ - . ~` remain unescaped. A bare native ID resolves only when it identifies exactly one session; otherwise the error lists the qualified candidates. Claude subagents are separate execution rows with a `(subagent)` parent indicator. A qualified selector matches exactly and never rolls up descendants.

```sh
agent-usage sessions --last 7
agent-usage session claude-code:session-123 --conversation
agent-usage session claude-code:session-123:agent-456 --conversation --format json
```

In the sessions viewer, select a row with arrows or `j`/`k` and press **Enter**. `Esc` returns to the same row; `q` quits. `--pager never` keeps normal linear output.

The reader shows user and assistant messages, with a turn outline on wide terminals. Tools and long output start collapsed. Use `Tab` to focus a block and Enter to expand it, `t` to show tools, `[`/`]` to jump turns, **m/M** to move between turns matching the selected model/mode, `/` to search text or model labels, `n`/`N` for matches, and `i` for session/source details. `r` reloads changed logs. Ctrl+C cancels loading and restores the terminal.

Claude conversations merge copied records by UUID and execution, join tool calls and results by call ID, keep thinking/redacted blocks visible as collapsed notices, label attachments, and report missing parents or results rather than inventing text. Conversations never change usage totals.

Piped output and `--pager never` produce the full conversation. JSON wraps `conversation` and the filtered `usage` report. CSV is for usage reports and is rejected for conversations.

### Quota status and forecasts

Reports and monitor show compact quota status when available. Quota capability is `recorded_snapshots` for Codex and `unsupported` for Claude Code. Claude allowance is never synthesized from tokens or costs. Sources stay separate because logs do not identify the account; their quotas are never added together.

A forecast is source-local: an unrelated Claude parser error or unreadable Claude source does not disable a complete Codex forecast. It does affect global `--strict` status. Forecasts use at most 60 minutes of loaded snapshots, require at least 2 minutes and 2 percentage points of quota consumption, and are unavailable for stale snapshots (over 5 minutes), large gaps, conflicting/partial contributing data, or a changing workload.

When recent observations are sufficient, `~6.6M tok left` estimates additional **input + output tokens** at the observed workload mix, and `exhaust ~` predicts when quota may run out. These are not guaranteed token allowances. If the reset arrives first, the display says `resets first`.

Quota processing never expands file discovery: default daily keeps its seven-day initial range with the existing two-day modification-time grace; monitor keeps today plus its 12-hour grace. Reset-relative daily periods use exact timestamps. JSON/JSONL include `quotas` and the period's `basis`; CSV remains usage rows only.

### Dates and filters

| Option | Meaning |
| --- | --- |
| `--today` | Today in the selected timezone |
| `--last N` | N calendar days including today; `--last 1` means today |
| `--since DATE --until DATE` | Inclusive date bounds, in `YYYY-MM-DD` format |
| `--all` | All retained usage |
| `--timezone ZONE` | Date boundaries, e.g. `Asia/Shanghai`; defaults to config or host timezone |
| `--agent NAME` | `codex` or `claude-code`; repeat to combine |
| `--model ID` | Exact recorded model ID; repeat to include several |
| `--mode fast` | Recorded mode: `regular`, `fast`, `flex`, `ultrafast`, `batch`, or `unknown` |
| `--session SELECTOR` | Qualified session selector or unique bare ID; repeat for several |
| `--project NAME_OR_PATH` | Recorded working directory or its final directory name |

Choose one date style: `--today`, `--last`, `--all`, or `--since`/`--until`.

```sh
agent-usage daily --since 2026-11-01 --until 2026-11-04
agent-usage monthly --all
agent-usage sessions --last 7 --project my-project
agent-usage models --today --mode fast
agent-usage models --all --group-by agent,provider,model
```

Daily reports show each date once. Every table ends with a **Total** for the full selection. Each date section ends with a **Subtotal**; monthly sections get their own subtotal. Session counts are unique within each period. JSON exposes subtotals in a separate array; CSV keeps only data rows so aggregates are not counted twice.

Custom grouping may deliberately combine agents or providers; the row's `agents` and `providers` membership sets make that unambiguous in JSON and CSV.

### Display and navigation

| Key | Action |
| --- | --- |
| Arrow keys or `j` / `k` | Scroll |
| Page Up / Page Down | Move a page |
| Home / End | Go to the first / last rows |
| `?` | Show help and accounting details |
| `c` | Toggle the monitor charts when space allows |
| `q` or Ctrl+C | Exit the viewer or monitor |

Use `--wide` for extra token breakdowns, `--exact` for full counts, and `--pager never` to print all rows into normal scrollback. Color adapts to the terminal; `--theme light`, `--theme dark`, `--color never`, and `--ascii` provide alternatives. `NO_COLOR` disables automatic color styling.

When Claude is selected, terminal views hide quota/reset information and default daily reports show Last 7 days. Codex-only views retain reset-relative defaults and quota details. Explicit date selections still take precedence.

## Live monitor

```sh
agent-usage monitor
agent-usage monitor --interval 5s
agent-usage monitor --agent claude-code
```

Monitor always summarizes **today**, in the selected timezone. It measures **Codex and Claude Code separately**:

- Each selected agent gets its own output-throughput chart, chart history, current speed, and token delta.
- The model table separates agents and providers.
- The per-session 60-second average remains independent.

The screen places today's summary and live rates at the top, then the agent charts, then model sections headed Codex and Claude. Model names have no added agent or provider prefix. A shared `Interval (2s):` heading introduces two-space-indented per-agent token/cost deltas; the duration follows `--interval`. Interval token counts use K/M/G abbreviations from 1,000 tokens (for example, `+1.2K` or `-3.4M`); `--exact` shows full counts. The full-selection total and controls stay at the bottom. Charts shrink or hide on small terminals; scrolling does not pause updates.

| Indicator | What it measures |
| --- | --- |
| `Today 00:00:00–HH:MM:SS` | Tokens and estimated cost from local midnight through the latest snapshot |
| `2s: +… tokens / +… USD` | Net changes since the previous refresh; `2s` follows `--interval` |
| Chart output | Each agent's output over an independent **2-second** sliding window |

Charts open with up to **180 points** reconstructed from today's already-loaded records—about six minutes at the default 2s interval. Historical points use logged timestamps; live points use observed arrivals. Dots show zero; `?` marks unavailable data. Midnight resets totals and chart history.

Logs can arrive in batches, so speeds describe observed throughput rather than exact streaming generation speed. Recovered backlog and corrections can change token/cost deltas but are excluded from fresh-output speed. A complete source outage creates a chart gap and restarts smoothing on recovery.

The default refresh is `2s`; `--interval` accepts seconds or milliseconds from `1s` to `3600s`. Historical date options such as `--last` belong to reports and are rejected by monitor.

## Cost estimates and token counts

Costs are **API-equivalent token estimates in USD**, using current bundled prices for all session dates. They are not subscription charges, an account balance, or an invoice. Tool fees, taxes, and negotiated discounts are excluded. Inspect the bundled snapshot with:

```sh
agent-usage pricing
agent-usage models --all --no-cost
```

The bundle includes verified OpenAI rates and, as of 2026-11-10, verified Anthropic rates plus common third-party Anthropic-compatible models used through Claude Code:

| Provider | Models |
| --- | --- |
| `anthropic` | Claude Fable 5.2/5.1/5, Opus 5.5/5/4.8/4.7/4.6, Sonnet 5.5/5/4.6, Haiku 5.5/4.5 |
| `deepseek` | DeepSeek Flash / V4.1 Flash, V4 Pro |
| `zhipu` | GLM 5.3/5.2/5.1/5, GLM 5.3 Flash, GLM 4.7/4.6/4.5 |
| `moonshot` | Kimi K3, K2.6, K2.7 Code |
| `minimax` | MiniMax M3, M3.1 Flash |

Claude transcripts do not record the backend provider, so a `claude*`, `deepseek*`, `glm*`, `kimi*`/`moonshot*`, or `minimax*` model namespace resolves the pricing scenario and marks it as an assumption. Set a source's `pricing_provider` to override that for unknown-provider events. A context hint such as `[1m]` is stripped only when the exact ID has no rate. Unknown aliases remain unpriced.

Input includes cached input; output includes reasoning. **Total = input + output**. Do not add the cached or reasoning columns again. Claude cache reads and cache writes are normalized into the inclusive input exactly once, with 5-minute and 1-hour write buckets priced separately. An unrecorded write duration remains unpriced unless an explicit unknown-duration rate exists.

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
cache_write_mode = "subset"
[rates.cache_write_rates]
"1800" = "3.75"
"43200" = "6"
```

```sh
agent-usage models --all --pricing-file ./rates.toml
```

TTL keys are positive integer seconds. `cache_write_unknown` prices writes whose duration was not recorded. Custom entries override bundled rates; unmatched usage still uses the bundle.

## Export

```sh
agent-usage daily --all --format json > usage.json
agent-usage daily --all --format csv > usage.csv
agent-usage monitor --format jsonl --frames 5 > live.jsonl
```

Reports and conversations use `schema_version = 2`. Source records include `agent`, quota `capability`, and local `coverage`. Rows include the requested dimensions and qualified session selectors. Counts include `agents` and `providers` membership sets; token meanings are unchanged. Structural monitor data uses `{ "session": { "agent", "native_id", "execution_id" }, … }` records rather than object keys, and `agent_activity` carries each agent's independent history.

CSV v2 prepends `schema_version`, `agent`, and `provider`, and adds JSON-encoded `agents` and `providers` columns. A grouped value is empty when the row aggregates across that dimension. Totals, subtotals, and diagnostics never appear as CSV data rows.

Redirected monitoring defaults to JSONL. Each line is a **complete snapshot**, so do not sum snapshots together. `--frames N` stops after N snapshots, including startup. Diagnostics go to stderr.

## Configuration and troubleshooting

On Linux/WSL, configuration normally lives at `~/.config/agent-usage/config.toml` (subject to XDG settings). Use `--config PATH` or `AGENT_USAGE_CONFIG` for another location. Command options are never saved. The configuration must declare version 2 and an agent on every source.

```sh
agent-usage sources
agent-usage doctor --no-cost
agent-usage doctor --source claude-linux --format json
agent-usage doctor --all --strict --no-cost
agent-usage monitor --help
```

- **A source is disabled:** enable its exact listed name, or select it explicitly with `--source`.
- **A Windows/WSL path is unavailable:** check the path from the system running this tool and make sure the distribution is accessible.
- **A price is missing:** inspect `pricing`, provide an override, set `pricing_provider`, or use `--no-cost`.
- **Claude quota is unsupported:** this is intentional; local Claude logs do not contain a reliable allowance snapshot.
- **Data is partial:** inspect `doctor` and per-source coverage. Deleted logs, incomplete writes, unsupported records, conflicting copies, and unreadable sources can limit coverage. Excluded usage is counted separately.
- **Imported logs seem missing:** date-limited reports filter file modification times before parsing. Reports allow a 48-hour buffer; monitor allows 12 hours before today's midnight. Use an all-time report to inspect imported logs whose old modification times fall outside the filter.

Session files are read-only. Reports reconstruct retained local data rather than querying an account.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --locked
```

Tests use temporary synthetic session logs; one optional local-log smoke test is ignored by default. Native Windows console appearance has not been separately checked.

## License

[MIT](LICENSE).
