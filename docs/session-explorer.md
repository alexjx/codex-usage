# Session conversation explorer

Status: implemented. The original approved interaction design is retained below.

Implementation notes:

- Conversation discovery currently walks bounded metadata headers on demand, rather than sharing an accounting-reader file index. It reads message bodies only for matching selected-session files and does not reuse report modification-time cutoffs.
- The selected transcript is decoded in memory; collapsed previews, incremental 2,000-line expansions, and viewport rendering limit screen work. Search and refresh run on cancellable workers. Large selected transcripts can still require substantial memory.
- Per-turn cost annotations remain optional and are not shown in this version. Session details use the authoritative selected-period usage snapshot and retain known partial cost subtotals.
- Plain/JSON exports include complete retained selected content rather than screen previews. Message text and tool arguments/results are sanitized for terminal control sequences.
- Verified with current/legacy synthetic records, plain/Zstd archives, copied/fork histories, real keyboard interaction and resizing. Native Windows suites cover compilation/parsing/CLI behavior; native console appearance is a separate visual check.

## Goal

Find a session in `sessions`, press Enter, and read the conversation without leaving the CLI. Keep the existing borderless appearance, semantic colors, responsive layout, and compact controls.

## Entry points

- `codex-usage sessions`: selectable session rows in the terminal viewer. Enter opens the selected session. All model/mode rows belonging to that session open the same conversation.
- `codex-usage session SESSION_ID --conversation`: open directly. Without `--conversation`, keep the existing usage report.
- `Esc` returns to the report with the selected row, scroll position, and filters intact. `q` exits the viewer; Ctrl+C also cancels loading and restores the terminal.
- Sessions reports open the interactive viewer even when short, so Enter is available. `--pager never`, pipes, and ordinary report exports retain linear usage output.

Only opening the conversation triggers message parsing. Listing usage must not load every session's prompts and tool outputs.

## Sessions report

Reuse the existing columns and totals. Add a selection indicator and highlighted selected row; avoid new diagnostic footer lines.

```text
Codex Usage / Sessions · Last 7 days · Asia/Shanghai

Session       Model         Mode       Input  Cached  Output  Est. USD
› 019abc…     gpt-6.1-sol    Fast        1.2M    1.1M   8.2K   $1.2345
  019def…     gpt-6.1-sol    Regular   850.0K  812.0K   4.1K   $0.4567

Total                                  2.1M    1.9M  12.3K   $1.6912
1–2/2   Enter Conversation   q Quit   ? Help
```

Numbers and session IDs in this document are illustrative.

Session selection uses the full stable ID, even when labels are shortened. Custom groups without a session dimension cannot be drilled into; their footer omits Enter Conversation. Selection follows rows rather than arbitrary wrapped lines. The selected session identity survives resizing.

## Conversation view

Use two header lines: session/project context, then date/source/turn position. Conversation text gets most of the screen. Keep a compact, pinned footer.

Wide terminal, at least 120 columns:

```text
Codex Usage / Sessions / 019abc… · codex-usage
Nov 04, 2026 · Asia/Shanghai · WSL + Windows · All retained conversation

Turns                        Conversation
  01  14:02  Review costs     14:04  You
› 02  14:04  Add a chart      Can we draw the output speed in the monitor?
  03  14:08  Smooth bursts
                             14:04  Assistant · gpt-6.1-sol · Fast
                             I'll add a chart below the live summary.

                             ▸ 3 tool calls · 2.1s

                             14:05  Assistant
                             The chart is ready to test.

Turn 2/3   [ ] Turn   t Tools   / Search   Esc Back   ? Help
```

- The turn list takes 24–32 columns; the conversation uses the remaining space. Turn labels come from the first nonempty line of the user message, never generated summaries or network calls.
- The selected turn stays highlighted in the outline as the reader scrolls; the outline scrolls only when the selection leaves its visible rows. Selecting a model/mode row opens a matching invocation, with nearby prompt context. `m`/`M` navigate matching turns while retaining the full transcript. `[` and `]` jump to previous/next turns; the outline itself scrolls to keep the selected turn visible.
- Below 120 columns, hide the outline and use the full width for conversation. Retain the turn-position indicator and jump keys.
- Below 60 columns, put role/time/model labels on separate wrapped lines and shorten controls. Very short terminals show a resize hint while preserving the return/quit actions.
- Wrap prose to the available pane width, capped at a readable 110 columns. Code blocks preserve indentation; long code lines wrap with a continuation marker rather than disappearing offscreen.
- Use accent for user labels/selection, bold body text for assistant labels, subdued metadata, the existing cost color for estimates, and warning color only for failed/unavailable items. Inherit terminal background; preserve no-color and ASCII fallbacks.

## Messages and tools

- Show user messages, assistant progress messages, and final assistant replies in their original order, grouped into turns where recorded IDs establish that relationship.
- A selected model/mode row opens a matching invocation in the report period; long prompts cannot hide the target below the viewport, and a matching tool call is revealed when needed. Without model focus, default to the latest retained user turn; date-filtered entry anchors to the first matching turn. Older retained context remains available by scrolling upward.
- Tool activity is a compact collapsed group between messages. Its summary shows call count and duration only when actually recorded. Expanded activity includes tool names, status, arguments, and results; code/output is visually distinct from chat text.
- `Tab`/`Shift+Tab` move focus between message/tool blocks. Enter expands the focused tool group or long message. `t` toggles tool-group visibility. Arrow keys and `j`/`k` scroll; Page Up/Down and Home/End retain existing meanings.
- Long messages/tool results start with a bounded preview and a visible expansion affordance. Expansion is cancellable and rendering stays limited to the viewport. Do not silently discard retained content.
- Show image/file attachment labels and available metadata; do not print image payloads or open links/files automatically.
- Internal setup/instruction records do not become ordinary chat messages. Encrypted or missing text is shown as unavailable where it creates a visible gap; never reconstruct missing prose or reasoning.
- A fork with retained parent context gets one collapsed `Earlier context` block, distinct from the selected session's own turns. Missing parent context gets a short placeholder, not invented history.

## Search and navigation

- `/` searches the loaded conversation locally; `n`/`N` go to the next/previous match. Highlight text and show match position only while searching.
- Search includes retained tool text and expands a collapsed block when opening a hit. Index/decode additional tool content lazily and show a cancellable search progress state.
- Escape clears an active search or closes an expanded overlay before returning to the sessions report. Preserve reading position when clearing search or resizing.
- This first version is a read-only explorer. Editing, replaying tool commands, and resuming a Codex session are separate future actions.

## Usage and cost scope

Conversation text and accounting use separate data paths:

```text
Selected sources → reconciled usage records → existing token/cost totals
Selected session → matching log files → message/tool blocks → conversation viewport
```

- Keep token totals/costs from the existing reconciled usage events. Never calculate usage from message text, tool-result counts, or message lengths.
- The report retains its selected date/model/mode filters. The conversation opens all retained messages for the selected session and source scope, preserving context. The header explicitly labels `All retained conversation` so users do not confuse the two scopes.
- A collapsed details panel, opened with `i`, shows full session ID, project, contributing sources/files, original report period, and usage totals for that period. Normal reader chrome stays compact.
- Per-turn token/cost annotations are optional details and appear only where usage has a reliable matching turn ID. Leave unavailable values as `n/a`; do not divide a session estimate evenly across turns.
- Reading a conversation or going Back preserves the current report and its options. Commands and options are not remembered across invocations; use `session ID --conversation` explicitly to open the reader. Navigation focus, search text, expansions, and scroll position remain transient.

## Data loading and integrity

Observed locally on both WSL and Windows logs: `response_item` contains message roles/content and function/custom tool calls/results; `event_msg` also contains lifecycle/item records, and `token_usage_record` has response/turn/thread identity. Inspection used record keys and types only, with no message text copied into this proposal. Formats are evolving; the explorer must tolerate older records and missing content.

- Build a session-to-file locator during the existing metadata scan without retaining conversation text. Once selected, read only contributing files, including retained compressed archives when needed. Conversation discovery must also find older files for that session outside the report's date/modification-time cutoff; use session IDs in paths and metadata to narrow that lookup. If complete retained history cannot be located, show that limitation instead of presenting it as complete.
- Match call outputs by call ID; keep source/file/sequence provenance. Use session and turn identity to distinguish copied records, fork context, and the selected session's own messages.
- Prefer canonical message records with stable IDs. Treat mirrored lifecycle records as alternate representations, not extra messages. Identical text alone is not a deduplication key: users can legitimately repeat prompts.
- Where stable identity is absent, reconcile only locally demonstrable mirrors within the same turn. Preserve ambiguous records with an inspectable provenance marker rather than dropping legitimate messages.
- Preserve within-file order; timestamps help merge sources, but equal timestamps do not justify reordering a tool result before its call. Scope uncertain cross-source ordering visibly in details.
- Pair tool calls/results even if a result arrives later. Mark an unmatched call as pending or interrupted only when lifecycle evidence supports it; a missing result alone means unavailable.
- Keep incomplete trailing JSON pending. Read files without modification. Sanitize control/escape sequences before rendering untrusted log content, and use the same Ctrl+C cancellation mechanism as existing readers.
- Do not add automatic follow mode in the first version. Show a compact `Updated on disk` hint and support `r` refresh; preserve the selected block while reading new records.

## Delivery slices and acceptance criteria

1. **Conversation data:** lazy selected-session parsing, typed message/tool blocks, provenance and fixtures for current/legacy formats, mirrored records, copies/forks, missing content, and partial writes. Existing accounting results must remain unchanged.
2. **Session selection and reader:** Enter/Back, one/two-pane layout, wrapping, tools, details and search. Verify 40/80/120/200-column terminals, short screens, Unicode/ASCII/no-color, large messages, and return to the exact report position.
3. **Direct command and integration:** `--conversation` on `session`, explicit handling of pipes (plain text or structured JSON; reject unsupported CSV), fresh-command defaults, errors and cancellation. On a pipe, emit complete selected conversation text unless explicit output limits are requested; a page preview must not silently truncate exports.

Prove the feature with synthetic conversation fixtures and actual terminal interaction before calling it complete. No user messages, logs, or local caches belong in the repository's published examples.

Confidence: high that the inspected logs support a reader and that the layout can reuse existing terminal infrastructure; medium for reliable cross-version conversation reconstruction until parsing fixtures and terminal tests pass.
