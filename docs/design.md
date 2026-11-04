# Codex Usage — Rust CLI design

Status: first CLI implementation completed, 2026-11-04; validation and practical limits are recorded in README.md and the implementation task review. This document retains the design rationale.

## 1. Purpose and scope

Build a local Rust executable, provisionally named `codex-usage`, that answers:

- Which models did I use, and how many tokens did each consume?
- How did that usage change by day, month, session, or project?
- How much came from Linux/WSL versus Windows?
- What is the estimated API-equivalent model cost, and which usage could not be priced?
- What new usage and estimated cost are being recorded while I monitor?
- Which records are missing, duplicated, or impossible to classify?

“Ingredients” is provisionally interpreted as token components: input, cached input, output, and reasoning. Attribution to individual prompts, skills, or tools is a separate possible feature: local aggregate token records do not establish how much each ingredient contributed.

Rust is the user's chosen language. Proposed architecture: a small synchronous executable with streaming file reads and no server or account login. This keeps the first release straightforward to distribute and inspect. These are design choices, not measured performance claims.

## 2. First-release features

| Feature | Proposed behavior | Reason |
| --- | --- | --- |
| Model summary | One row per exact recorded model identifier and recorded mode | Avoid merging distinct model versions or inventing backend identities |
| Time views | Daily and monthly, with separate model rows | Show changes while retaining model detail |
| Session view | One row per session/model pair; drill into a session | A session may switch models |
| Source configuration and view | Set, enable, and select Windows and local WSL paths independently; combine or split reports | Allow either source alone and make combined reports explainable |
| Project filter | Match recorded working directory; allow explicit project aliases | Windows and Linux path spellings can differ |
| Token detail | Show input, cached input, output, reasoning, and total | Explain totals without counting subsets twice |
| Cost estimates | Per-model/mode USD estimates at current rates, custom overrides, and pricing coverage | Make usage comparable while exposing assumptions |
| Live monitoring | `monitor` refreshes per-model tokens and costs from both configured sources | Follow new usage without rerunning commands |
| Exports | Table, JSON, CSV | Support terminals, scripts, and spreadsheets |
| Diagnostics | `sources` and `doctor`, plus on-demand coverage details | Make incomplete reports visible |

Defer graphical dashboards, automatic pricing downloads, account quota tracking, and multi-tool usage ingestion. Cost estimation and terminal live monitoring are first-release requirements, built on the same verified token accounting as historical reports.

## 3. Proposed command interface

```text
codex-usage                             # current calendar month, grouped by model
codex-usage models --all                # all retained history, grouped by model
codex-usage daily --since 2026-11-01 --until 2026-11-04
codex-usage monthly --all
codex-usage sessions --today
codex-usage session <session-id>
codex-usage models --source windows --model '<exact-model-id>'
codex-usage models --group-by source,model
codex-usage daily --project codex-usage --timezone Asia/Shanghai
codex-usage models --format json --exact
codex-usage daily --format csv
codex-usage daily --all --pager never    # complete output in normal scrollback
codex-usage models --pricing-file ./rates.toml
codex-usage models --no-cost
codex-usage monitor                     # today plus changes since monitor startup
codex-usage monitor --interval 5s --source windows
codex-usage monitor --session <session-id>
codex-usage monitor --format jsonl       # continuous snapshots for scripts
codex-usage pricing                     # inspect rate version, sources, overrides
codex-usage sources
codex-usage sources set wsl --kind wsl --root /home/xinj/.codex
codex-usage sources set windows --kind windows --root /mnt/c/Users/xinj/.codex
codex-usage sources disable windows
codex-usage sources enable windows
codex-usage models --source wsl
codex-usage monitor --source wsl --source windows
codex-usage doctor
codex-usage --root /path/to/.codex models
```

Common options:

- `--today`, `--all`, `--last N`, or `--since/--until` for reports; reject contradictory selections.
- Date-only bounds include both named dates in the selected timezone. Internally use start-of-first-day through start-of-day-after-last-day, excluding the final instant.
- `--timezone <IANA-name>` defaults to the host timezone and is printed in every report. Read enough earlier history to calculate baselines before filtering usage events.
- Repeatable `--source`, `--root`, and `--model`; model matching is exact, with an explicit separate search option if needed later.
- Source names select independently configured paths. No selection means all enabled sources; repeated `--source` values select exactly those named sources for this invocation. Reject unknown names and combining `--source` with ad hoc `--root`.
- `--group-by` supports model, mode, source, day, month, session, and project. Keep model in default report groupings.
- `--sort total|input|output|cost|model`; default total descending with stable model-name tie breaking. Cost sorting places unpriced rows last and preserves partial markers.
- `--exact` shows full integers; `--wide` adds detail columns. JSON and CSV always contain exact integer counts.
- `--format table|json|csv` for historical reports; monitor supports table and JSONL. `--color auto|always|never`, `--strict`.
- `--pager auto|always|never` controls the internal read-only viewer for human-readable historical reports. Auto opens it only for an interactive terminal when the complete rendered report exceeds screen height. Piped output and JSON/CSV always bypass it; reports never silently limit rows. Monitor uses its own live screen.
- `--theme terminal|dark|light` selects semantic colors without changing the terminal background; `--ascii` replaces Unicode glyphs. Automatic color respects `NO_COLOR`, `TERM=dumb`, and piped output. Machine-readable formats never contain styling.
- Costs are shown by default. `--no-cost` hides cost columns; `--pricing-file <path>` overlays explicitly supplied rates on the bundled table. Unknown pricing remains visible as `n/a`.
- `--pricing-date YYYY-MM-DD` explicitly reprices usage at the rates for that date; current bundled rates are the default for all session dates.

Bare invocations reuse the last successful usage report and its options; explicitly naming a report starts fresh with the calendar-month range. `--last N` selects N calendar days including today in the selected timezone. Monitor always summarizes today, follows local midnight, and rejects historical ranges. Help text and headers state these defaults to prevent accidental assumptions about all-time totals.

## 4. Table interface

The visual specification is [Terminal visual design](cli-ui.md), covering borderless layouts, semantic colors, long-report navigation, a running monitor, width adaptation, and fallback behavior. A Rust interaction preview is available at `examples/ui_preview.rs`, built through `examples/ui-preview/Cargo.toml`; it uses fictional data and does not implement session ingestion. The content examples below define accounting detail; the visual specification governs presentation.

Default screens are compact: title/context, source status, one token/cost summary, data rows, and a total row; diagnostic counters are available on demand. Move accounting explanations, rate provenance, and the full key legend behind `?` or `--help`. Monitor adds a compact rolling output-speed indicator (`out tok/s`) beside its since-start deltas, plus the mean per-active-session output speed over 30 seconds. Individual session rates are available in JSONL. See the visual specification for observation windows and exclusions; throughput never includes preloaded history, recovered backlog, or deduplication corrections.

Illustrative data and model names only; these are not measurements of the user's sessions.

```text
Codex Usage | 2026-11-01–2026-11-04 | Asia/Shanghai
Sources: wsl, windows | local retained logs

Model         Sessions       Input     Cached*     Output    Reason*      Total    Est. USD
model-alpha          3     100,000      60,000      20,000     12,000    120,000     $0.3800
model-beta           2      50,000      10,000      10,000      4,000     60,000     $0.0900
unknown              1       2,000           0         500        n/a      2,500         n/a
TOTAL                5     152,000      70,000      30,500     16,000†   182,500    $0.4700‡

* Cached is included in Input; Reason is included in Output.
† Known reasoning only; at least one record lacks this breakdown.
‡ Priced subtotal only: 180,000 / 182,500 tokens priced (98.6%).
Pricing: illustrative custom rates | API-equivalent model cost
Sessions total counts distinct sessions; model rows may overlap.
Coverage: 2 sources read | 0 unreadable files | 0 conflicting records
```

The example intentionally includes a session shared across two model rows: model session counts do not add up to the distinct-session total. When a subset field is unavailable, show `n/a`; show partial aggregate values with a marker and JSON coverage counts, never silently treat missing as zero. Illustrative rates per million tokens are input/cached/output = 3/1/10 for model-alpha and 1/0.2/4.8 for model-beta; cache-write counts are explicitly zero. These are fictional rates, not claims about OpenAI prices.

Daily reports show one human-readable date heading above each group of model/mode rows, retaining full per-row dates in exports. Source splits add Source. Session detail prints full session ID, source provenance, recorded project, first/last counted activity, model rows, and any accounting warnings.

Use borderless tables with spacing instead of lines or grids. Keep numeric columns right aligned, model IDs bold, titles/token-share bars cyan, costs violet, healthy source labels green, warnings amber, and errors red. Colors supplement explicit labels. Full model IDs wrap rather than collide or become ambiguous. At narrow widths, the default table reduces to Model/Total/Est. USD, then stacked blocks below 50 columns, with the same explanatory footer; `--wide` prints all columns even if horizontal scrolling is necessary. `--no-cost` removes the estimate column. No dependence on emoji; automatic color is disabled when piped. Use the terminal's own font and background.

For long ranges, compute all selected rows and totals. The report viewer pins the period summary, column headings, and row-position/help footer while its data viewport scrolls. Whole-period totals never change merely because a different page is visible. Arrows or `j`/`k` scroll; Page Up/Page Down page; Home/End reach the first/last rows; `q`/Ctrl+C closes the read-only viewer and restores the terminal. Resizing reflows the viewport around the same logical row. `--pager never` prints all rows in normal scrollback; no output mode silently drops rows or changes requested daily granularity to make a report fit.

Machine-readable stdout contains only the chosen format; diagnostics go to stderr. JSON includes `schema_version`, period, timezone, sources, rows, totals, and coverage. Cost values are exact decimal strings or null; include pricing status, assumed dimensions, rate-table version, and priced/unpriced token counts. CSV has fixed flat columns and a header; emit totals and diagnostics separately rather than mixing row types.

Historical-report exit codes: 0 for a successful report (possibly with disclosed warnings), 1 for read/parse failure under `--strict` or inability to read any selected source, 2 for invalid arguments/configuration. An accessible but empty source produces a valid empty report and exit 0. Monitor's retry and termination behavior is specified separately below.

## 5. Independent Windows and WSL sources

Official documentation establishes that `CODEX_HOME` contains Codex state and defaults to `~/.codex`. Windows/Linux bridging below is behavior proposed for this tool, not a Codex guarantee.

Windows and local WSL are separate source entries, each with its own name, kind, root path, and enabled setting. Neither depends on the other being configured or accessible. One Windows root and one local WSL root are the common setup; additional named profiles or distributions use additional entries. Source kind describes the configured environment; it does not reinterpret a path or prove where copied usage originated.

Selection and precedence:

1. Explicit `--root` values replace configured and discovered roots for that invocation, with deterministic temporary source names. Reject combining them with `--source`.
2. Explicit `--source <name>` selects exactly those configured entries, even if disabled for default reports; this temporary selection does not change saved settings. Do not read unselected roots.
3. Otherwise, when source configuration exists, select all enabled entries. If none are enabled, report `No sources enabled` with exit 2 and configuration guidance; do not silently fall back to local data.
4. Only when no source configuration exists, use `CODEX_HOME`, or the native user's `.codex` directory, as the native root. Name it `wsl` on WSL, `windows` on Windows, or `linux` on other Linux hosts.
5. `sources` can list accessible opposite-side candidates separately from configured entries; discovery never enables or saves a source automatically. This avoids silently including other Windows profiles or WSL distributions.

For this workspace, the observed roots are `/home/xinj/.codex` (local WSL) and `/mnt/c/Users/xinj/.codex` (Windows). Once both are configured and enabled, default reports and monitor combine them. `--source wsl` reads local WSL alone; `--source windows` reads Windows alone; repeated flags combine the two. Selection applies before cross-source deduplication, so a copied event is attributed to its selected provenance when reporting either source alone, and counted once when combining them.

Source management commands:

The same source catalog drives listing and enable/disable lookup. A discovered or implicit source can be enabled by its displayed name; enabling registers it, preserving the native implicit source when the first discovered folder is added. Discovered names are disambiguated from existing configured names and duplicate roots are hidden. Listing does not persist or enable candidates.

```sh
codex-usage sources set wsl --kind wsl --root /home/xinj/.codex
codex-usage sources set windows --kind windows --root /mnt/c/Users/xinj/.codex
codex-usage sources                  # configured paths, enabled state, readability
codex-usage sources disable windows  # default reports use WSL alone
codex-usage models --source windows  # temporary explicit selection still works
codex-usage sources enable windows   # default reports combine both again
```

`sources set` creates or updates one named entry without altering other entries. New entries default to enabled; updating an entry preserves its enabled state. Accept kinds `wsl`, `windows`, and `linux`; require a kind for a new entry and preserve it on updates unless explicitly changed. Store absolute paths as interpreted by the running host, resolve relative CLI paths against the invocation directory, and write configuration atomically. A missing or temporarily inaccessible root can be saved with a clear warning so setup does not depend on mounted drives being available. Duplicate names or aliases resolving to the same root are configuration errors; runtime copies across distinct roots still use normal deduplication.

`sources` shows configured name, kind, full path, enabled state, and current accessibility. Discovery candidates appear in a separate labeled section. `doctor` reports issues for selected/enabled roots; a disabled, unselected Windows source does not make a WSL-only report fail. Historical reports disclose failures in selected roots; monitor retries them using its source-health rules. A running monitor pins its selected roots until restarted, just as it pins rates.

On WSL, probe the current user's accessible mounted Windows profile candidates without assuming every mount is `/mnt/c`. Allow explicit paths for non-default mounts and custom Codex homes. On native Windows, support explicit accessible `\\wsl.localhost\<distro>\home\<user>\.codex` roots. Do not start stopped distributions or execute profile scripts as a side effect of reporting. Other Linux hosts scan only native/configured accessible roots.

Within each root, inspect `sessions/` and `archived_sessions/`. Do not scan auth files, databases, attachments, or unrelated directories. Detect `.jsonl.zst` inputs; either support streaming decompression before release or explicitly mark them as unsupported coverage—never silently omit them. First-release acceptance requires compressed-log fixtures if such logs are present in the target data.

Proposed configuration, located in the native OS application config directory:

```toml
timezone = "Asia/Shanghai"

[[sources]]
name = "wsl"
kind = "wsl"
root = "/home/xinj/.codex"
enabled = true

[[sources]]
name = "windows"
kind = "windows"
root = "/mnt/c/Users/xinj/.codex"
enabled = true
```

Configuration paths are host-specific. A PowerShell installation uses Windows/UNC paths. A source label describes storage provenance, not necessarily the operating system where usage originated. Keep all source origins on deduplicated usage; assign a deterministic primary source for additive source tables and disclose multi-source records.

## 6. Accounting contract

Data flow:

```text
source selection → roots → files → parsed metadata/events → normalized usage increments
      → deduplication → filters → grouped totals → table/JSON/CSV
```

The parser isolates evolving log formats from aggregation. A normalized increment retains session identity, event time, recorded model/provider/tier if available, token counts, file/record provenance, and evidence quality.

Local inspection found two schema generations. Recent top-level `token_usage_record` events have per-response `usage` alongside cumulative `turn_token_usage` and `thread_token_usage`. Use the incremental `usage` once, linking its `turn_id` to the matching `turn_context` model. Do not sum the cumulative fields. Preserve `response_id`, `session_id`, `thread_id`, and `turn_id` for deduplication. Older `event_msg` / `token_count` records require the cumulative-delta path below. If both representations describe the same request, reconcile them rather than adding both; prefer the identified per-response record.

Recent usage records also contain `cache_write_input_tokens`. Preserve it in normalized data and exports, with an optional wide-table column. Its absence is unknown rather than zero. Verify its subset and pricing semantics separately before using it to derive any total or estimate.

Rules to validate against fixtures before implementing:

1. Read metadata and model changes in file order, retaining a turn-to-model lookup. Assign identified responses to their matching turn; otherwise use the active recorded model where evidence permits, not the session's final model or today's configuration. Keep missing model as `unknown`; preserve routing aliases literally.
2. For legacy records, treat cumulative snapshots as counters, not independent requests. Count advancing deltas; repeated snapshots contribute zero. A counter decrease starts an explicitly diagnosed new segment, not a negative increment or an automatic charge of the full snapshot.
3. Never add both `last_token_usage` and a cumulative delta for the same event. Prefer a validated cumulative delta; use latest-request fields only when the parser can establish a distinct, previously uncounted request. Put ambiguous usage into coverage diagnostics.
4. Input includes cached input; output includes reasoning. `total = input + output`. Validate reported totals, subset bounds, missing fields, and nonnegative counts. Preserve discrepant raw values for diagnostics instead of silently correcting them.
5. Deduplicate copies across active/archive roots and OS sources using session identity plus stable event/request identity where available. For logs without stable request IDs, compare ordered record prefixes and provenance; do not deduplicate independent requests merely because counts match. Merge diverging copies only where evidence permits; report conflicts.
6. Forks and child agents can contain inherited history. Establish inherited prefixes/baselines before counting child increments; identical token values alone do not prove inheritance. Count genuinely new child usage once. If the format cannot establish the boundary, report uncertain coverage rather than presenting a precise total.
7. Inspect separately recorded compaction usage. Count a distinct compaction request only with evidence it is not already included in token snapshots or inherited history. A compaction marker without token counts cannot supply missing usage.
8. Use event timestamps for time grouping, not filenames, file modification times, or session start dates. Filter after constructing deltas and inherited baselines, including parents outside the selected date range.
9. Attribute a delta spanning a model boundary without sufficient per-request evidence to `unknown`, with a diagnostic. Unknown usage remains in totals.
10. Ignore unknown unrelated event kinds, but warn on unrecognized usage-bearing structures. An incomplete final line in a live log is deferred and disclosed; malformed complete lines are counted as parse failures. Strict mode fails on incomplete coverage.

Model aliases cannot reveal unrecorded backend models. Retained local logs cannot establish account-wide usage, remaining subscription quota, or an exact invoice.

## 7. Cost estimation

Cost estimation ships in the first release and appears by default in historical reports and monitor. The scope is API-equivalent model-token cost in USD. Subscription charges, quota consumption, taxes, tool fees, and negotiated discounts are outside this estimate.

### Rate selection and provenance

- Bundle a versioned, reviewed rate table with official source URLs, verification dates, and documented effective intervals where known. Reporting and monitoring remain offline; a local file can provide reviewed updates without upgrading the binary. The `pricing` command lists supported models, dimensions, source dates, and overrides.
- Select current rates per request using exact model/provider, recorded service tier, context bracket, and cache retention where applicable. Session dates do not change the pricing basis. Different requests in one model row can use different rates; calculate first, then aggregate. Never infer a request's context bracket from daily or thread totals.
- For known models without recorded tier, show an explicitly labeled Standard-tier assumption, not a claim about the historical tier. Missing context/retention may use a documented default assumption only when a sensible model-specific estimate is possible. Record every assumption; otherwise leave the affected usage unpriced.
- Per user preference, all session dates use the current verified pricing snapshot. These are estimates at current prices, not reconstructed historical charges. The snapshot date and source are retained in exports.
- `--pricing-file` overrides matching model/provider/dimension/date entries. Validate decimal rates, nonnegative values, and overlapping/ambiguous matches at startup. Missing override entries fall through to the bundle. Reject a conflicting match rather than choosing silently. Preserve custom-rate provenance.
- Keep model aliases as recorded; pricing mappings require an explicit reviewed or user-supplied mapping and an estimate marker. Do not guess a backend model from an alias or use today's config to reconstruct historical settings.

### Calculation and incomplete data

For a request with no separate cache-write charge:

```text
estimated USD = ((input - cached_input) × input_rate
                + cached_input × cached_rate
                + output × output_rate) / 1,000,000
```

Reasoning is already included in output. Models with cache-write pricing require separate handling: after verifying that write tokens are a disjoint subset of input, partition input into ordinary, cache-read, and cache-write tokens, and apply each published category rate once. If a provider defines an additive write fee instead, apply its documented formula. Never add the full write price on top of an input charge for the same tokens without that evidence. Unresolved cache-write semantics make the affected request unpriced; missing write counts on a model with write pricing require a disclosed assumption or an unpriced result.

Apply published context/tier/retention/regional distinctions only through explicit rate rules, with any missing dimensions recorded as assumptions. Use checked decimal arithmetic; aggregate before rounding the display to four decimal places. Display a positive estimate below $0.0001 as `<$0.0001` rather than zero. Exports preserve unrounded decimal strings.

Unknown rates produce `n/a`, never zero. A partially priced row or grand total shows a marked **priced subtotal**, priced/unpriced token counts, and the proportion priced. All-unpriced usage yields `n/a` for cost, including the total. Distinguish `priced`, `assumed`, `partial`, and `unpriced` in exports, with a list of reasons and assumptions. `--strict` fails on assumptions or incomplete pricing when costs are enabled; `--no-cost` skips pricing checks.

Pin the pricing table for the lifetime of a monitor process, including the hash of custom overrides. Restart monitoring to apply changed rates, so live cost changes cannot be confused with new usage.

## 8. Live monitoring

`monitor` watches all configured sources and renders a combined per-model table. It uses the exact same normalization, deduplication, and pricing as historical reports. This is live observation of retained log records: costs and tokens become visible when Codex writes usage, not necessarily while it is still generating a response. Do not extrapolate missing usage or predict spend.

### Commands and display

```text
codex-usage monitor --interval 2s
codex-usage monitor --source wsl --source windows
codex-usage monitor --session <session-id>
codex-usage monitor --group-by source,model
codex-usage monitor --format jsonl --color never
```

Support source/model/project/root/timezone/pricing filters from reports, plus repeatable `--session`, `--interval <duration>` (default 2s, minimum 1s), and `--format table|jsonl`. Monitor rejects `--all`, `--last`, `--since`, and `--until`; its range is always today. At local midnight its summary, since-start deltas, and output-speed samples reset for the new date.

Illustrative live display using the same fictional rates as the historical table:

```text
Codex Usage MONITOR | Today: 2026-11-04 | Asia/Shanghai
Sources: wsl OK, windows OK | Refresh: 2s | Last scan: 10:42:16

Model          Input     Cached*    Output     Total    Est. USD
model-alpha  100,000      60,000    20,000   120,000     $0.3800
model-beta    50,000      10,000    10,000    60,000     $0.0900
TOTAL        150,000      70,000    30,000   180,000     $0.4700

Since ready 10:40:00: +12,000 tokens | +$0.0310 estimated USD
Pricing: illustrative custom rates | 100% priced | 0 assumptions
Cached is included in Input. Wide view adds Reason and Cache write.
Watching for new usage | Ctrl+C to stop
```

On startup, establish an initial reconciled snapshot and show `Ready` with its timestamp (metadata within JSONL, never a free-text line mixed into that stream). Since-ready values measure the net changes to the selected report after that baseline; late-written historical records can therefore contribute and are labeled late arrivals. Deduplication corrections may decrease totals; show signed corrections instead of clamping them to zero. At midnight reset monitor deltas to the new day's totals and clear the speed window; do not retain previous-day summary values. Restarting monitor establishes a fresh baseline.

In an interactive terminal, monitor enters the alternate screen and redraws in place rather than appending each refresh to scrollback. Pin the summary/source status, column headings, and footer; use a bounded, independently scrollable data viewport when rows exceed available height. Summary totals cover all selected rows, not just the visible page. Use report navigation keys without pausing updates; retain the current row while values change and do not force the viewport to newly added rows. Keep model order stable by name by default and mark recently changed rows without relying only on color. `--sort` explicitly selects a different order. Recompute layout on resize without overflowing the screen; extremely short terminals show a compact resize prompt while collection continues.

Show per-source last successful scan, delayed/new usage, pending partial records, errors, pricing coverage, and visible row positions. An idle log is not evidence of an offline source: distinguish `no new usage` from `source unreadable`. At narrow widths show Model/Total/Est. USD. Ctrl+C or `q` stops monitoring, restores the previous screen/cursor/input mode, and prints a final timestamped summary. Cleanup also runs on ordinary errors and unwinding panics. The preview may redraw a complete bounded frame; the production renderer should update changed lines where practical.

When stdout is not a terminal, default to JSONL without redraw controls. Explicit table mode prints timestamped snapshots without ANSI cursor movement. JSONL writes one complete snapshot per refresh, starting with the baseline and ending with a final snapshot, with sequence number, observation time, period, rows, totals, since-ready changes, source health, pricing provenance, and coverage. Records are snapshots, not additive events. Reject unsupported JSON/CSV streaming formats with a useful message. Backpressure must not produce partial JSON lines or unbounded queues.

### Reading new data safely across Windows and WSL

- Poll configured directories for new/moved files every refresh; tail only appended bytes in unchanged plain logs, keeping offsets, partial-line buffers, metadata, and counter baselines in memory. Polling is the first-release approach so cross-filesystem operation does not depend on native watch notifications. Measure discovery cost before promising latency on large corpora.
- Reports and monitor skip files last modified more than 48 hours before their selected period starts, before opening content. Unknown modification times remain eligible; all-time reports scan all files. File metadata is rechecked each refresh, so old sessions resumed today are included. Event timestamps still enforce the exact report period. Selected files are parsed from the beginning before the ready baseline to preserve metadata and cumulative counters. Changes during initialization are reconciled using file identity/size checks and a catch-up scan; partial final lines remain pending. The filter assumes modification times reflect writes; imported logs with preserved earlier modification times may require an all-time export.
- Detect replacement, truncation, archive moves, and rewritten prefixes using identity/fingerprint checks, not path alone. Invalidate affected contributions and reconcile against the shared record ledger; the final result must match a fresh historical report. Same-size, mtime-preserved rewrites are verified progressively by a background sweep. A sweep starts 60s after initial scanning or the previous sweep completes; completion time depends on corpus size, not a hard 60s guarantee. Ordinary size/mtime/identity changes are checked on each refresh.
- For supported compressed logs, reread changed files safely, deduplicating unchanged requests. Keep an incomplete compressed stream pending. Do not treat decompression failures as zero usage.
- A temporarily unreadable source retains its last known totals with a stale marker; retries occur on later refreshes. New readable sources are reconciled and their backlog is labeled, not treated as usage generated at the observation time. If all sources are unreadable, keep monitoring with a prominent stale status; strict mode exits on coverage failure.
- The refresh interval is a polling target, not a guarantee. Never overlap scans; report actual scan duration and lag when a scan overruns. Graceful termination preserves final coverage status; hard failures and strict violations exit 1.

## 9. Rust implementation boundaries

Proposed modules: `cli`, `config`, `sources`, `parse`, `accounting`, `pricing`, `aggregate`, `monitor`, and `render`. Start with one crate, buffered line-by-line input, integer token counts with overflow checks, decimal currency arithmetic, deterministic collections/order, and a normalized event type. Choose maintained crates and verify their current interfaces during implementation.

Keep all reporting offline. Preserve only metadata needed for aggregation and diagnostics; do not export prompts, responses, or credentials. Avoid a persistent cache in the first release. Measure the real corpus before adding caching or concurrency; cross-OS filesystem access can affect performance.

## 10. Verification and release gates

Implementation acceptance requires redacted synthetic fixtures, not private transcripts committed to the repository:

- Model changes inside one session yield correct separate rows and a reconciled total.
- Repeated cumulative snapshots count once; subset columns never inflate totals.
- Copied Windows/Linux logs and active/archive copies count once.
- Windows-only, WSL-only, and combined configurations select exactly their own roots; disabling one source never triggers reads or failures from it unless explicitly selected. Source updates preserve other paths and enabled states. Cover unknown names, zero enabled entries, missing mounts, duplicate roots, ad hoc root conflicts, and source selection before deduplication.
- Diverging copies, counter resets, unknown models, and missing fields are disclosed.
- Forked/subagent inherited history and compaction fixtures avoid duplicate charges.
- Date filters retain the necessary earlier baseline, including parent sessions; timezone midnight and daylight-saving boundaries are tested.
- Truncated live files, malformed records, inaccessible roots, and empty history have defined outputs and exit codes.
- Table, JSON, and CSV totals agree; narrow output preserves full model identity.
- Visual checks cover borderless report/monitor layouts at 40/60/80/100/140 columns, long model IDs, unknown prices, large counts, readable dark/light/terminal palettes, and explicit text for each status. Verify NO_COLOR/TERM=dumb/piped output, forced-color precedence, ASCII mode, ANSI-free exports, and numeric alignment after styling.
- Multi-page reports preserve complete rows and whole-period totals, pin headings and scope, expose final rows with Home/End and paging keys, and handle wrapped identities and resize without unreachable content. Auto-pager bypasses short reports; never-pager and piped modes emit complete linear output without cursor controls.
- Pricing fixtures cover ordinary input, cached input, verified cache writes, reasoning subsets, tier/context changes within a model row, dated rates, explicit repricing, custom overrides, unavailable rates, partial coverage, and rounding after aggregation.
- Synthetic cost examples reconcile: model-alpha $0.3800, model-beta $0.0900, priced subtotal $0.4700; 180,000 / 182,500 tokens priced rounds to 98.6%.
- Deterministic monitor tests simulate appends, partial lines, newly created files, archive moves, rewrites, truncation, duplicate cross-OS copies, source outages/recovery, initialization races, and midnight rollover. After each reconciliation, monitor totals match a fresh report using the same filters and pinned rates.
- Monitor output tests verify valid complete JSONL snapshots, sequence and baseline semantics, signed corrections, strict-mode errors, piped output, backpressure, and terminal restoration on Ctrl+C.
- Interactive monitor tests verify a bounded screen across multiple changing frames, navigation while updates continue, stable viewport identity, resize, all-row summary versus visible-row counts, and restoration after `q`, Ctrl+C, and injected errors. Run the synthetic interaction demo in a real terminal/PTY; a static screenshot cannot demonstrate a live loop.
- Windows build and a native Windows run validate drive paths/UNC paths; WSL run validates both configured sources. Linux compilation alone does not prove Windows support.

Use `cargo fmt --check`, `cargo clippy`, and `cargo test` once implementation exists. Compare a small manually audited session to CLI output. Treat ccusage output as a useful secondary comparison rather than accounting ground truth.

## 11. Evidence and confidence

| Statement | Basis | Confidence |
| --- | --- | --- |
| Rust is the implementation language | Explicit user preference | High |
| `CODEX_HOME` is the Codex state root, defaulting to `~/.codex` | Official environment-variable documentation fetched 2026-11-04 | High |
| Both native and Windows Codex roots exist here | Read-only filesystem inspection; Windows active and archived JSONL files found | High for this environment only |
| Evolving formats, forks, compaction, and cumulative counters require care | ccusage's own Codex documentation plus local schema inspection notes | High that these need coverage; not a guarantee every format is understood |
| Proposed commands and module boundaries will suit this workflow | Design judgment; requires user review and implementation validation | Medium |
| Pricing needs model-specific cache, context, and tier rules | Official API pricing and model pricing notes fetched 2026-11-04 | High; historical rate coverage and log completeness still require validation |
| Polling is a suitable initial cross-platform monitor approach | Design judgment to avoid dependence on cross-filesystem notifications; no benchmark yet | Medium |

Sources:

- [OpenAI: Environment variables](https://developers.openai.com/codex/config-file/environment-variables)
- [ccusage: Codex data source](https://ccusage.com/guide/codex)
- [OpenAI: API pricing](https://developers.openai.com/api/docs/pricing)
- [OpenAI: model-specific pricing notes](https://developers.openai.com/api/docs/models/gpt-6.1-sol)

ccusage's documentation contains conflicting legacy-model inclusion statements. This design therefore does not copy those fallback behaviors; unknown model usage remains visible and unguessed.
