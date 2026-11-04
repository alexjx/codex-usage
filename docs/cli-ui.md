# Terminal visual design

Status: implemented visual specification; the production CLI is documented in [README](../README.md). A separate Rust preview remains available for long reports and a running monitor. All preview data and prices are fictional; the preview never reads sessions.

## Direction

A compact usage ledger for developers comparing model usage and cost across Windows and WSL. Use generous separation between sections, aligned numbers, and short source/status labels. No outer box, cell grid, background panels, gradients, emoji, or required custom font. Typography comes from the user's terminal: bold title/model identities, normal-weight numbers, and secondary metadata with readable contrast.

The signature is a small bar beside each model showing its share of total tokens, not a decorative chart. Pair it with a numeric percentage and reserve it for wide output so essential token and cost columns get priority. Cost remains visually distinct without coloring higher spending as an error.

## Color roles

Use the terminal's own foreground/background by default. Never paint a background or depend on a dark terminal. Default `terminal` theme uses the user's ANSI palette; `dark` and `light` provide explicit RGB alternatives for users who know their background. Automatic truecolor detection is best effort; fall back to ANSI colors when unsupported.

| Role | Terminal palette | Dark alternative | Light alternative | Use |
| --- | --- | --- | --- | --- |
| Accent | Cyan | #67D4E8 | #00677D | Title and token-share bars |
| Cost | Magenta | #C4B5FD | #6B3FA0 | Estimated USD column and priced subtotal |
| Healthy | Green | #86D3A5 | #17643D | Source status with the word `ready` |
| Caution | Yellow | #F0C674 | #805600 | Partial pricing, stale sources, assumptions |
| Error | Red | #F28B82 | #B3261E | Failed reads or invalid configuration |
| Metadata | Default foreground | #A8B3C7 | #526075 | Dates, headings, explanatory notes |

White/gray body text follows the terminal foreground. Do not assign every model a different color: full model IDs establish identity. Keep input/output numerals neutral so the table remains easy to scan. Healthy status appears on source labels, never as an implication that a price is cheap. Warnings always include text and affected counts.

`--color auto|always|never` applies to human-readable output; auto respects non-TTY stdout, `TERM=dumb`, and the presence of `NO_COLOR`. An explicit `--color always` overrides those automatic choices. Machine-readable exports never contain escape codes, even with color forced. Add `--theme terminal|dark|light` and `--ascii`; ASCII replaces bars with `#` and `.`, while `--color never` removes styling without changing content. No blinking, pulsing, or screen-wide flashes.

## Report composition

Every table report has a bold Total row at the bottom, aligned with the displayed data columns; the Mode cell is empty because the total spans modes. In the viewer and monitor this row is pinned below the data and always covers the full selection, not the visible page. The total is the last report line; diagnostic counter footers are hidden. Narrow layouts wrap the total without losing requested wide detail.

Default screens prioritize data. Keep normal chrome to a title/context line, one source-status line, and one combined token/cost summary. Monitor adds one compact delta/speed line. No permanent explanatory paragraphs, pricing provenance, debug ticks, repeated stale notices, or full keyboard legend. Keep detailed accounting notes in `--help` and behind `?` in interactive views. A demo label in the title is sufficient to identify fictional data.

1. Title and report name, with concise date range/timezone in production.
2. Selected source names/statuses. Full paths belong in `sources` or `doctor`.
3. One summary line: total tokens and estimated USD; `subtotal` when incomplete.
4. Borderless table: two spaces minimum between columns; model left aligned, counts/money right aligned. Column labels use sentence case. Separate the total row with whitespace and bold weight.
5. Do not show unpriced-token, data-warning/pending, or pricing-assumption counter lines in normal reports or monitor. These details belong in `?`, `doctor`, and JSON exports. The viewer footer contains position plus `q Quit  ? Help`; `?` toggles navigation, subset semantics, rate details, and speed definition. Show a stale source once in its status label. Detailed diagnostics remain available via help/doctor and exports.

Exact-count wide mockup (plain text cannot display the intended colors):

```text
Codex Usage  /  Models · Nov 01–04 · Asia/Shanghai
wsl ready  ·  windows ready
182,500 tokens · $0.4700 est. subtotal

Model          Token share       Input   Cached   Output     Total   Est. USD
model-alpha    ███████░░░ 65.8%  100,000    60,000   20,000   120,000    $0.3800
model-beta     ███░░░░░░░ 32.9%   50,000    10,000   10,000    60,000    $0.0900
unknown        ░░░░░░░░░░  1.4%    2,000         0      500     2,500        n/a

Total                           152,000    70,000   30,500   182,500   $0.4700

```

The cost column and summary use the Cost color; the unpriced notice uses Caution. Model IDs are bold, metadata is secondary, bars use Accent. Total is bold with an extra blank line, without drawing a border. Show full model IDs and unknown/unpriced rows; wrap only the identity cell and leave continuation numeric cells blank. Percentages can round to slightly over 100%; bar lengths are approximate and their numeric labels carry the values. Subset explanations and pricing provenance are available on demand instead of occupying permanent rows.

Default human output may abbreviate large counts consistently, e.g. `120.0K`; `--exact` expands them to integers with separators. Currency uses four decimal places and preserves partial markers and tiny-positive handling. Exports always preserve exact counts and unrounded decimal amounts.

## Terminal widths

Choose columns from measured display width after stripping styling, allowing room for the actual model IDs and values. The ranges below are targets, not permission to overflow; long IDs may force a smaller layout. Unicode width must use display cells, not byte counts.

- At about 116 columns and above: full model identity, token-share bar/percentage, input, cached input, output, reasoning, total, and estimate. `--wide` adds sessions and cache writes as space permits or uses explicit horizontal overflow.
- At about 88–115 columns: model, input, cached input, output, total, estimate. Omit the share bar and reasoning before shrinking essential columns.
- At about 50–87 columns: model, total, estimate. Below-table note names omitted breakdown columns and points to `--wide` or JSON for details.
- Below 50 columns: stacked model blocks with total and estimate on a second line. Wrap identity on its own line, never silently truncate or introduce ambiguous ellipses.

`--no-cost` removes cost values and pricing notices and redistributes the freed space. Keep data-quality details available on demand rather than in the normal footer. Check 40, 60, 80, 100, and 140 columns, plus long model IDs and large numbers.

## Modes and date sections

Show a separate Mode column: Fast in the accent color, Regular in normal text, Unknown when no tier was recorded. Default grouping separates model/mode combinations. Explicit mode-free grouping may display Mixed; pricing still happens per request. Never infer historical mode from today's configuration.

Daily tables show one heading such as `Wed, Nov 04 2026` above each date's model/mode rows, with spacing between dates. Do not repeat the date in each model identity. While that heading is above the viewport, pin its date context once; do not duplicate a date heading that remains visible. JSON/CSV keep ISO dates per row. Monthly reports use month headings.

## Long reports and date ranges

A year of daily model rows must remain readable without silently losing data. Compute the selected report in full and show its whole-period summary; the viewport only changes which rows are visible. `Total` always means the full selected period, never the current page. The footer shows a compact position, e.g. `41–60/1095  q Quit  ? Help`; the selected date range stays in the title, and total scope is explained on demand.

Use an internal read-only viewer, without launching an external pager:

- `--pager auto` (default): print short reports normally. If both stdin and stdout are interactive terminals and the rendered report exceeds the available height, open the viewer. Count wrapped lines and all headers/footers when deciding whether it fits.
- `--pager always`: open the viewer even for a short report when the terminal supports it. With redirected input/output or `TERM=dumb`, fall back to complete linear output; never wait for keyboard input in a pipeline.
- `--pager never`: print every row into normal scrollback, suitable for terminal search, copy/paste, redirection, or an external pager. No cursor movement or alternate screen.

The viewer uses the alternate screen with a pinned compact context/whole-period summary at the top, column headings below it, and a pinned row-position/help footer. Only the data region scrolls. Day/month groups retain their labels when their first row is outside the viewport; do not infer a row's date from an offscreen heading. Long identities wrap, and scrolling moves by rendered line so no wrapped content becomes unreachable. No `top N` limit, hidden rows, collapsed dates, or automatic aggregation that changes the requested report.

Controls: arrows or `j`/`k` move one line; Page Up/Page Down move one viewport; Home/End go to the beginning/end; `q` or Ctrl+C closes the viewer successfully. Show only `q Quit  ? Help` in the footer; `?` reveals the complete controls. Search and interactive filters are later enhancements; existing command filters select the dataset before viewing. Closing restores the previous screen, cursor, and terminal input mode, then prints a compact whole-period summary in normal scrollback. The report remains reproducible with `--pager never`.

On resize, recompute columns and page height and keep the same logical row near the top. Clamp offsets when the viewport grows. If the terminal is too short for the pinned chrome plus one data line, replace the viewer with a compact `Enlarge terminal to view rows; q Quit` state; never erase the dataset or corrupt input modes. Default piped output is complete and ANSI-free, with no repeated per-page headers. Explicit forced text color follows the color option; it never enables cursor-control sequences or paging in a pipe. JSON/CSV always export all selected rows and never use a pager, regardless of `--pager`.

Example proposed production commands:

```sh
codex-usage daily --since 2026-01-01 --until 2026-12-31
codex-usage daily --all --pager never
codex-usage daily --all --format csv > usage.csv
```

## Live monitor

Monitor always summarizes today in the selected timezone. Date-range options are hidden from monitor help and rejected if supplied. At midnight, the summary, deltas, and speed samples reset to the new day. Historical ranges and `--last N` belong to report commands.

The summary begins `Today 00:00:00–HH:MM:SS`, showing local-midnight start and latest snapshot time; the title supplies date/timezone. Cost includes all selected usage recorded today, including before monitor launch. The activity prefix follows `--interval`: `2s:` by default, or the configured duration (including fractional seconds). Token/cost deltas contain only changes since the previous snapshot; the next idle refresh resets them to zero. Today's summary remains cumulative. Signed corrections and recovered records affect these observed net deltas; cost uses the known priced subtotal. Unpriced-only changes display n/a; mixed changes in one refresh show a marked known subtotal. The parenthesized per-session average retains its independent 60-second window. JSONL provides window_tokens/window_cost_usd/window_cost_partial/configured window_seconds/actual sample_seconds alongside cumulative since_ready_* fields. Source/parsing gaps add an inline partial-data badge, not warning counters; filtered monitor scope adds FILTERED in the title, with exact filters in help. Incomplete empty data must not appear as a free zero-cost day.

Use the same palette, headers, table alignment, and coverage rules. Add a text `LIVE` label in Accent, actual last-scan time, and refresh target. Do not animate a dot to imply fresh data when no usage has arrived.

```text
Codex Usage  /  Monitor LIVE · Today
wsl ready  ·  windows stale (last read 10:41:52)
180,000 tokens · $0.4700 est.
+12,000 tokens · +$0.0310 · 600 out tok/s (80.0 tok/s)

Model              Input   Cached   Output     Total   Est. USD
model-alpha      100,000    60,000   20,000   120,000    $0.3800
model-beta        50,000    10,000   10,000    60,000    $0.0900

1–2/2  q Quit  ? Help
```

Current speed is newly observed output since the previous snapshot divided by actual elapsed seconds. Target refresh cadence and activity label come from `--interval` (default 2s); collection overruns are included in measured sample time and never cause overlapping scans. A second indicator, the parenthesized rate `(… tok/s)`, averages individual session rates over a 60-second rolling window, or elapsed startup time if shorter. Only sessions with fresh output within that window contribute; idle sessions expire and zero active sessions yields zero. JSONL exposes individual session rates, mean, configured interval and actual sample duration. Exclude input/cache tokens and preexisting records loaded at startup; show zero on idle refreshes. Production monitoring must exclude recovered backlog, copied records, and corrections from throughput. Label it `out tok/s`: retained log writes can arrive in bursts, so this is observed output throughput, not a claim about the model's streaming generation speed. Put that explanation in `?` help, not the normal screen. The demo measures its synthetic output increments with a monotonic clock.

Stale source text is amber, not green. Pricing coverage concerns observed tokens; source freshness is a separate status and must not disappear behind a `100% priced` label. Recently updated rows receive a short `+` marker and one-frame bold emphasis; no motion or color-only signal. Signed corrections use `Correction` text rather than presenting negative values as failed reads.

Monitor is a running screen, not a report repeatedly appended to scrollback. In an interactive terminal it opens the alternate screen, keeps the summary/source health, table headings, and footer pinned, and replaces the visible data in place at each update. More models or grouped rows than fit create a scrollable data viewport using the same arrows/`j`/`k`, Page Up/Page Down, and Home/End controls as reports. The footer shows `x–y/N  q Quit  ? Help`; summary tokens/cost always include all selected rows, including rows offscreen. The monitor has its own viewport; `--pager` governs historical reports only.

Scrolling does not pause collection or refresh. Retain the top logical row while values change, keep model ordering stable by default, and append newly discovered model identities without pulling a reader away from their current position. Explicit user sorting may reposition rows; retain the current identity where possible and disclose new rows in the footer. Avoid automatic follow-to-bottom behavior. Recompute the bounded viewport after resize without writing below the terminal bottom. At small sizes use the same compact/suspended state as the report viewer while collecting data continues.

Redraw changed lines where practical, retaining column widths within a layout. A full replacement redraw is acceptable for the preview, provided it homes the cursor and updates the bounded screen rather than clearing scrollback or printing one new screen per tick. `q`/Ctrl+C stops collection, restores the previous screen, shows the cursor, restores input mode, and prints a final timestamped summary. Apply cleanup on ordinary errors and unwinding panics as well. With non-interactive output, do not enter raw mode or the alternate screen; use the complete timestamped snapshot/JSONL behavior in the main design instead.

## Empty, loading, and failure states

- Loading: `Reading sessions…` on stderr for interactive terminals; replace it with the report when ready. No spinner in piped output.
- Empty: `No usage recorded for this period.` Show selected sources and suggest `--all` or checking `sources`; no zero-filled dummy model rows.
- Partial read: `Windows unreadable · showing WSL only` with a short error and coverage count. Never present partial usage as a complete total.
- No readable sources: explain the selected paths and suggest `codex-usage doctor`. Monitor may wait/retry as defined in the main design.

## Preview and verification

`examples/ui_preview.rs`, built through `examples/ui-preview/Cargo.toml`, is a Rust interaction preview, not the production CLI. It uses fictional model names and prices, never discovers sources, and never opens session logs. The long-report demo supplies enough synthetic daily rows to exercise the viewer; the monitor demo changes synthetic usage and source status over time.

```sh
cargo run --manifest-path examples/ui-preview/Cargo.toml -- --long
cargo run --manifest-path examples/ui-preview/Cargo.toml -- --long --pager never
cargo run --manifest-path examples/ui-preview/Cargo.toml -- --monitor --interval 2s
cargo run --manifest-path examples/ui-preview/Cargo.toml -- --monitor --theme light
cargo run --manifest-path examples/ui-preview/Cargo.toml -- --monitor --frames 5 --color never --ascii
```

Run the interactive commands from a terminal to see scrolling and in-place updates; captured tool output cannot show the continuous screen experience. `--frames N` limits the monitor demo to a finite number of snapshots; it does not mean a row limit. Optional preview `--width`/`--height` controls support layout demonstrations. `--pager auto|always|never`, color/theme, and ASCII options allow fallback comparisons. Production input parsing, full rate provenance, source polling, and accounting remain outside the interaction preview.

Before release, verify a multi-page daily report reaches its final row without changing full-period totals; navigation through wrapped identities; short-report auto-pager bypass; piped complete output; fixed monitor screen height across ticks; scrolling while updates continue; resize in both directions; and cleanup after `q`, Ctrl+C, and an injected error. Check both themes, no-color output, and the existing width/data edge cases. The design choices have medium confidence until reviewed in a real user terminal; executable validation results belong in the task review, not an unverified claim in this specification.
