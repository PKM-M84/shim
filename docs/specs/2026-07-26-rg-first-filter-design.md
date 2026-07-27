# Design: invert the pipeline — rg first, ast-grep as a filter

Date: 2026-07-26
Status: approved (Chris, 2026-07-26)
Affects: `src/main.rs`, `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`
Follows: v0.3.13 (flag fidelity, PR #15) and v0.3.14 (report truth, PR #16) — both
are prerequisites; this work reuses `OutputMode`/`render_output` and the
unsupported-flag deny-list from v0.3.13.

## Problem

Only 10.7% of intercepted searches actually redirect to ast-grep, and failed
redirects outnumber successful ones. Measured over 1,379 events (30 days, all
`agent=claude-code`) in `~/.smart-rg/stats.db`:

| outcome | count | share |
|---|---|---|
| passthrough | 967 | 70.1% |
| fallback (ast-grep empty) | 249 | 18.1% |
| structural (win) | 147 | 10.7% |
| ast_grep_error | 16 | 1.2% |

### Not all of this is a defect

Of the 900 `not_structural` passthroughs:

| class | count | verdict |
|---|---|---|
| contains a regex alternation `a\|b\|c` | 700 (78%) | genuinely a text search |
| escaped-paren call search (`store_mls_message\(`) | 29 | **false negative — a real bug** |
| plain lowercase word (`federation`, `worktree`) | 81 | deliberately rejected as too broad |

So the honest target is not "redirect more". A large share of what agents search
for is legitimately text: alternations, SQL identifiers, env-var names, strings.
The target is **decide correctly, and stop paying twice when we cannot.**

### The two real defects

**1. Language is guessed from the filesystem, not from the data.**
`infer_lang_from_path` walks the search path and picks one dominant language.
On a polyglot repo the symbol usually lives in another one. The event log shows
`arming_snapshot` attempted as python (5×), javascript (4×) and typescript (2×)
— eleven empty runs for one symbol. Same shape for `worker_capable`
(javascript, 4×) and `frameworkReconnectCommand` (javascript, 3×).

**2. Every miss costs two process spawns.**
A failed redirect runs ast-grep, gets nothing, then runs real rg. That is 249
double-runs. Note that a *successful* redirect also costs two spawns today
(ast-grep, then `run_rg_count` for the comparison baseline) — so the current
pipeline already pays twice in nearly every structural case. Spawn count was
never where the savings lived.

A third group cannot be fixed by any amount of routing care: symbols that are
not identifier nodes at all. `AV_WORKER_OS_ISOLATED`, `tenant_id`,
`app.current_tenant_id`, `hub_identities`, `bypass_rls` live in string literals,
SQL migrations and env config. ast-grep structurally cannot match them, and the
agent genuinely wants the text hits.

## Decision

Invert the pipeline. Run ripgrep **first** — it is ground truth and can never be
silently empty — then use ast-grep to **filter** its hits down to the ones that
are real structural matches.

```
args → parse → stdin / pattern-less / unsupported-flag / --no-smart
                                       └→ exec real rg, verbatim    (unchanged)
     → classify(pattern) says text     └→ exec real rg, verbatim    (unchanged)
     → classify says structural:
         ① run real rg CAPTURED with --json
            (user's filter flags kept; output-mode flags stripped)
         ② rg found nothing  → print nothing, exit 1     ← ast-grep never spawned
         ③ rg found hits:
             a. languages := extensions of the files rg ACTUALLY hit
             b. per language: ast-grep -p <translated> -l <lang> <just those files>
             c. confirmed := line spans ast-grep reported
             d. confirmed empty → print ALL rg hits, log `fallback`
             e. else → print rg hits ∩ confirmed, stderr note for the rest
         ④ log comparison: rg hits vs confirmed — exact, and free
```

### Why this addresses the measured failures

| today's failure | why it disappears |
|---|---|
| `arming_snapshot` tried as py/js/ts, empty ×11 | languages come from the files that *matched*, not a filesystem walk |
| 249 fallbacks at two spawns each | a search rg cannot find never reaches ast-grep (step ②) |
| `run_rg_count` third spawn for the baseline | rg already ran; the comparison is a byproduct |
| `files_saved` structurally pinned near 0 | confirmed files are now a true *subset* of rg's files, so a file whose only hits were comments really is saved |
| ast-grep and rg disagreeing on ignore rules | ast-grep receives an explicit file list, not a directory |

Spawn count is unchanged for the common case (2 either way) and strictly lower
for the failures. The larger gain is epistemic: today an empty ast-grep result is
ambiguous — wrong language, wrong pattern form, or genuinely absent? Inverted, rg
has already established ground truth, so an empty ast-grep result means exactly
one thing: *these hits are not structural.* That is a fact worth reporting rather
than a failure to recover from.

## Filter policy

Print the structurally-confirmed lines on **stdout**; write a note about the
suppressed ones to **stderr**:

```
stdout:
  src/db.py:41:await bypass_rls(conn)
  src/api.py:88:bypass_rls(session)

stderr:
  smart-rg: 12 text-only matches suppressed (comments/strings/SQL) — rerun with --no-smart
```

stdout stays clean for parsing; the suppression is visible and recoverable, never
silent. This matters precisely because of the `bypass_rls` / `tenant_id` class —
those hits are real and wanted, and an agent that sees the note can re-run.

## Detailed semantics

### Output rendering

rg runs internally with `--json`, keeping the user's *filtering* flags (`-g`,
`--type`, `-w`, paths) and stripping output-mode flags (`-c`, `-l`, `--json`,
`--heading`, `-n`/`-N`). The filtered set is then rendered through the existing
`OutputMode` enum from v0.3.13 — `Content { line_numbers }`,
`Count { show_filename }` and `FilesWithMatches` are reused unchanged.

Stripping `-n`/`-N` from the *replay* loses nothing: v0.3.13's parser already
records the caller's preference on `RgInvocation.line_numbers`, and rendering
reads it from there. (`-o` and the rest of the v0.3.13 deny-list never reach this
path at all — those calls force passthrough before any of this runs.)

`--json` is chosen over parsing `path:line:text` because a path may contain a
colon, and because the shim already parses ast-grep's JSON — the two are
symmetric.

### Matching ast-grep hits to rg lines: containment, not equality

A structural node can span several lines while rg reports each matching line
separately:

```python
result = bypass_rls(          # rg hits here; ast-grep node STARTS here
    session, tenant_id        # rg ALSO hits here for `tenant_id`
)
```

Intersecting on start-line alone silently drops the second hit. The rule is
**containment**: keep an rg hit when its line falls anywhere within some
confirmed node's `[start, end]` span. ast-grep's JSON supplies both ends; note
its `range.start.line` is 0-indexed (v0.3.13 already normalises to 1-based in
`parse_ag_matches`, and `range.end.line` needs the same treatment).

This is the same class of silent-drop bug that produced v0.3.9, v0.3.10 and
v0.3.12, so it gets a dedicated regression test.

### Exit codes

| situation | exit |
|---|---|
| rg found nothing | 1 (rg's own code) |
| confirmed set non-empty | 0 |
| confirmed empty, rg had hits (all printed) | 0 |
| rg errored | passthrough, rg's code (unchanged) |

### `--no-smart`

A shim-consumed flag that forces plain passthrough. It **must be stripped before
forwarding**, because it is not an rg flag and rg would reject it. This needs
explicit handling: v0.3.13's parser deliberately treats unknown flags as harmless
booleans, which is right for real rg flags but wrong for this one.

No env-var equivalent. The stderr note tells the caller the flag exists; that is
the whole recovery path.

### `classify()` — escaped-literal rule

Today `classify()` returns false on any backslash, which rejects all 29
escaped-paren call searches. But `translate_pattern` already strips backslashes,
so the information was there all along.

New rule — for every `\X` in the pattern:

- **X non-alphanumeric** (`\(`, `\)`, `\.`, `\[`) → an escaped literal; unescape it.
- **X alphanumeric** (`\b`, `\d`, `\w`, `\s`) → a regex assertion; passthrough.

After unescaping, any remaining *unescaped* `|`, `[`, `*`, `+`, `?`, `{` means a
real regex → passthrough.

| pattern | result |
|---|---|
| `store_mls_message\(` | → `store_mls_message(` → structural ✓ |
| `app\.current_tenant_id` | → `app.current_tenant_id` → structural ✓ |
| `\btext\(` | rejected (word-boundary assertion) |
| `a\(\|b\(` | rejected (alternation) |

### Telemetry

Three honest outcomes rather than today's two:

- `structural` — ast-grep confirmed a subset (a win)
- `fallback` — rg hit, ast-grep confirmed none
- `no_match` — rg found nothing; ast-grep never ran. Neither a win nor a failure,
  and counting it as either would distort the redirect rate.

Comparison rows are logged only when rg found hits, with exact counts on both
sides. `estimated_*` fields keep their current meaning.

### Memory bound

rg output is captured rather than streamed. The shim already does this with
ast-grep's JSON, so it is not new in kind — but rg matches more, so it is new in
degree. Cap the captured set at **10,000 matches**: on exceeding it, stop
filtering and render the captured rg matches unchanged, logging `fallback` with
reason `over_cap`.

Note this must NOT re-exec rg — rg has already run, and its output is in hand.
Discarding it to spawn a second identical search would reintroduce exactly the
double-run this design exists to remove. A search returning more than 10k hits
is not one where noise-trimming is the user's problem anyway.

## Testing

**Unit**
- containment: an rg line inside a multi-line confirmed node is kept; a line
  outside every span is dropped
- the escaped-literal rule, across the four cases tabulated above
- rg `--json` parsing → `RgMatch { file, line, text }`
- `--no-smart` is detected and removed from the forwarded args
- the 10k cap trips into passthrough

**End-to-end**
- polyglot fixture (`.py` + `.ts` both containing `arming_snapshot`) proving the
  language now resolves from the hit files rather than a dominant-language guess
- multi-line call fixture for containment
- comment/string fixture proving suppression + the stderr note
- **regression**: re-run the v0.3.13 nine-flag matrix and confirm those calls
  remain byte-identical to real rg

## Out of scope

The 700 alternation patterns stay passthrough. Generating ast-grep YAML `any:`
rules for them is a separate project, and most of those terms are SQL or string
identifiers ast-grep cannot match anyway.

Raising the redirect rate is explicitly **not** the success metric. The metrics
that should move are: fallbacks-per-win (down), spawns on a no-match search
(2 → 1), and wrongly-rejected call searches (29 → 0).
