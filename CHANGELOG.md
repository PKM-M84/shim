# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.15] - 2026-07-27

### Changed — invert the pipeline: ripgrep runs first, ast-grep filters its hits

Measured over 1,379 events (30 days, live `agent=claude-code` traffic): only
10.7% of intercepted searches actually redirected, and failed redirects
(18.1%, ast-grep came back empty) outnumbered wins. Two real defects, not one:

- **Language was guessed from the filesystem, not from the data.**
  `infer_lang_from_path` picked one dominant language for the whole search
  path, so on a polyglot repo the symbol usually lived in a language that
  wasn't chosen — `arming_snapshot` alone was attempted as python (5×),
  javascript (4×) and typescript (2×) before ever finding its actual `.ts`
  file. `group_files_by_lang` now derives language from the extensions of the
  files ripgrep **actually hit**, so every language present in the results
  gets its own ast-grep pass, run once each rather than guessed once.
- **`classify()` rejected the canonical way an agent writes a call search.**
  An escaped literal like `store_mls_message\(` was 29 of 900 "not
  structural" passthroughs — a real false negative, not a deliberate
  rejection. `literal_form` now reduces an escaped pattern to its literal text
  before classifying it, so `\(`, `\)`, and `\.` read as the structural
  signals they are, while `\.env` (a leading/trailing dot) still reads as a
  text search.

The pipeline itself is now: parse → passthrough checks (stdin, pattern-less,
unsupported flags) behave as before, plus the new `--no-smart` → `classify`
says text, forward verbatim, unchanged → `classify` says structural:

1. Run real ripgrep **captured** with `--json`, the user's filter flags kept,
   output-mode flags stripped.
2. Zero hits → print nothing, exit 1, log `no_match/rg_empty` — ast-grep is
   never spawned for a search that was always going to be empty.
3. Hits found → run ast-grep once per language against just the files that
   matched, confirm each ripgrep hit by **containment**: does its line fall
   anywhere inside a confirmed node's `[start, end]` span, not only at the
   node's start line (a multi-line call's continuation-line arguments used to
   be silently dropped by start-line equality). A file `group_files_by_lang`
   can't map — `.sql`, `.md`, unmapped extensions — is never handed to
   ast-grep, so it has no standing to suppress those hits; they're kept in
   full. A language that confirms nothing is treated as unsearched for the
   same reason, rather than as a wrong-grammar wipeout.
4. All of a language's hits confirmed-empty → print every ripgrep hit for
   those files and log `fallback`, honestly, instead of returning nothing.
5. Otherwise → print the confirmed hits, and for the rest print a single
   stderr notice (`N matches not confirmed as structural by ast-grep — rerun
   with --no-smart`) instead of silently deleting them.

- **New: `--no-smart`.** Forces plain ripgrep with no structural filtering,
  for when you want every text match back. It is the remedy the suppression
  notice points at, and it is stripped before forwarding so ripgrep never
  sees a flag it does not recognise.

Every successful redirect previously cost two spawns anyway (ast-grep, then a
second rg run — `run_rg_count` — just for the comparison baseline); this
inversion doesn't add a spawn on the win path and removes one on every
zero-hit search. `run_rg_count` and the old flag-limited `run_ast_grep` are
both deleted; `run_ast_grep_on_files` (explicit file list, not a directory
walk, so ripgrep and ast-grep can no longer disagree about ignore rules)
replaces them.

Covered by `cargo test` (119 tests) — including containment against a
synthetic multi-line node span, the one case a realistic fixture cannot
produce, since ripgrep matches the very text that seeds the ast-grep node —
and end-to-end against the release binary: polyglot symbols found in every
language they appear in, unsearched-file-type hits kept in full,
comment/string hits suppressed with the stderr notice, `--no-smart`
restoring ripgrep's raw output, and a true no-match exiting 1 and logging
`no_match/rg_empty`. The v0.3.13 nine-flag passthrough matrix (`-v`,
`--invert-match`, `-A2`, `-C 1`, `-i`, `-m1`, `-o`, `-F`,
`--files-without-match`) remains byte-identical to real ripgrep.

### Fixed — two shapes of the inversion that silently dropped real hits

`explicit_pattern` keeps exactly one `-e`/`--regexp` and discards the rest —
harmless before this release, when that single pattern only decided WHETHER
to redirect. Once `capture_argv` started forwarding every pattern token to
the rg capture, ripgrep began answering the *union* of all of them while
ast-grep kept filtering against the one pattern that survived, silently
deleting every hit that only matched a *different* one:

- **`rg -e A -e B`** (the flag spelling of an alternation, already out of
  scope for redirect) now forces passthrough instead of returning only the
  hits that matched `A`.
- **`rg -f patterns.txt`** now forces passthrough instead of filtering
  against the patterns *file's own name* — the parser stored the filename in
  `explicit_pattern`, so a search for what the file listed was silently
  answered as a search for the string `patterns.txt`.

Both are semantics a single ast-grep pattern cannot express, so both now set
`inv.unsupported` and go to real ripgrep whole — the same mechanism that
already covers `-v`, `-C`, `-i`, and the rest of the deny-list. Verified
byte-identical to real ripgrep on both shapes against the release binary.

### Fixed — two ways the stats told on themselves

- **The redirect rate no longer counts searches ripgrep answered with
  nothing.** A `no_match` (rg found zero hits, so ast-grep never ran) is
  neither a redirect nor a passthrough; counting it in the denominator
  dragged the headline rate toward zero for any run of genuinely empty
  searches. `no_match` now has its own bucket in `smart-rg stats` and its own
  series in the report's time chart, and the rate describes only searches
  that had something to route.
- **`events.lang` can name several languages for one search**, now that
  ast-grep runs once per language actually present in the results. The "By
  Language" breakdown and "Top Redirected Patterns" now split that list
  before counting, instead of treating `python,typescript` as a language of
  its own, distinct from `python` and `typescript`.

## [0.3.14] - 2026-07-26

### Fixed — the report's headline numbers describe the whole dataset

- **Every savings KPI was a page, not a total.** The headline figures were
  summed inside the `ORDER BY id DESC LIMIT 50` query that feeds the detail
  table, and `report.html` set "Comparison Runs" from `comparisons.length` —
  the length of that page. So the KPI read exactly `50` for any database past
  50 rows, and the savings beside it covered only the newest 50 while the
  counters next to *them* were all-time. Totals now come from a separate
  whole-table aggregate. On a live 402-row database:

  | KPI | was | now |
  |---|---|---|
  | Comparison runs | 50 | 402 |
  | Files saved | 104 | 4,748 |
  | Noise avoided | 734 | 760,733 |
  | Est. cost saved | $0.02 | $22.82 |

  This is also why earlier fixes "worked and then quit": a sliding
  most-recent-N window re-decays as rows land. The window is gone, not retuned.
- **`smart-rg stats` had the same bug** — it summed the printed rows and
  labelled the result "Total".
- **Generating a report silently deleted data.** `compute_stats` ran a lazy
  30-day `prune_old_events` on every `stats`/`report` invocation, while
  `comparisons` was never pruned — one page mixed a 30-day event window with an
  all-time comparison table. Viewing a report no longer mutates the database;
  retention remains available as the explicit `smart-rg prune` command.
- **The page now states its window** ("All data since YYYY-MM-DD"), and the
  detail table says when it is showing a subset.

### Fixed — the savings metric compares like with like

- **The ripgrep baseline counted LINES; ast-grep counts OCCURRENCES.**
  `run_rg_count` used `--count` (matching lines) against ast-grep's node count,
  so two occurrences on one line scored rg=1 vs ag=2. That produced rows where
  the text tool appears to find *less* than the structural one — 96 of 402 rows
  on a live database — and systematically understated the noise avoided. Now
  uses `--count-matches`.
- Combined with the flag fidelity fixes in 0.3.13 (all paths and `-g` globs
  forwarded, unsupported flags never redirected), both sides of a comparison
  finally measure the same search. Previously `rg PATTERN src other` searched
  only `src` with ast-grep, then booked the unsearched directory as
  `files_saved`.

## [0.3.13] - 2026-07-26

### Fixed — a redirected search must answer the question that was asked

`run_ast_grep` honoured only `-c` and `-l`; every other flag was silently
dropped, so a redirect could answer a *different question* than the caller
asked. Measured against real ripgrep on a fixture (see #14).

- **`-v` returned the exact opposite of the truth.** An inverted search
  reported the *matching* lines instead of the non-matching ones. Flags whose
  semantics ast-grep cannot reproduce now force passthrough with a logged
  `unsupported_flag_<flag>` reason: `-v`/`--invert-match`,
  `--files-without-match`, `-A`/`-B`/`-C` (context), `-o`, `-m`,
  `-i`/`-S` (ast-grep matching is case-sensitive), `-F`, `-r`, `-q`, `--json`,
  `--iglob`. This is a small deny-list of *semantics*, not a re-enumeration of
  rg's flag surface — unknown flags stay harmless as before.
- **Every redirected hit pointed one line too high.** ast-grep's JSON
  `range.start.line` is 0-indexed and was printed raw; ripgrep reports
  1-indexed lines.
- **Only the first path was searched.** `rg PATTERN src other` silently
  dropped `other/` — and then *credited the miss as a saving*, because
  `files_saved` compared rg's full walk against ast-grep's partial one.
  `ast-grep run` takes multiple `[PATHS]`, so all of them are now forwarded.
- **`-g/--glob` had no effect.** Forwarded to ast-grep's `--globs`, which takes
  the same gitignore-style syntax including `!` negation.
- **`-c` printed a bare total.** ripgrep prints `path:count` per file, and a
  bare count only for a single explicitly-named file. Both shapes now match.
- **The line-number field was emitted unconditionally.** Piped `rg PATTERN src`
  prints `file:content`; `-n`/`-N` are now honoured, defaulting to ripgrep's
  own rule (on for a TTY, off when piped).

Note: 0.3.10–0.3.12 shipped without changelog entries; this entry does not
backfill them.

## [0.3.9] - 2026-06-10

### Fixed — stop hijacking pattern-less rg modes (the real "stats quit again" bug)

- **`rg --files <path>` is no longer treated as a search.** The flag-agnostic
  parser took the path positional as the *pattern* (only the zero-positional
  case was recognized as pattern-less), and `classify()` called any token
  containing a `.` structural — so dotted paths like `~/.claude/plugins/cache`
  were redirected to ast-grep with the **path as the search pattern**. The
  agent's file listing silently came back empty (exit 1), and every such call
  wrote a junk row: 124 of 206 "structural redirects" and 67% of all
  comparison rows were phantom path-searches. `--files` / `--type-list` now
  mark the invocation pattern-less: all positionals are paths, pattern stays
  `None`, and the call forwards to real rg verbatim, unlogged.
- **`classify()` rejects path-like tokens.** A `/` cannot appear in an
  identifier or call pattern in any supported language; patterns containing
  `/` (paths, slash-bearing regexes like `from.*invites/(A|B)`) now always
  pass through to rg. Defense-in-depth behind the parser fix.
- **Why stats "quit once again":** the report's headline KPIs sum the 50 most
  recent comparison rows. Each junk row crowded a real one out of that window,
  so every prior fix looked good for a few days and then the dashboard decayed
  back toward zero as garbage accumulated. Fixing the garbage *source* makes
  the window self-heal; `smart-rg prune` docs cover clearing old junk rows.
- Covered by `cargo test`: pattern-less parsing (`--files` with paths and
  value flags, `--type-list`) and path/regex/structural classification.

## [0.3.8] - 2026-06-06

### Fixed — pattern translation & language inference (the last two redirect gaps)

- **Function-definition searches now match.** `fn main(` was translated to the
  call form `fn main($$$)`, which matches nothing (a body-less item isn't a
  complete node). Adding a body (`fn main($$$) { $$$ }`) only matched functions
  *without* a return type — every `fn …(…) -> T {` and `function …(): T {` was
  still missed. A definition (`fn`/`function`/`func` keyword followed by a name)
  now translates to the bare `keyword name` signature, which ast-grep matches
  against the whole function item **regardless of return type or body** — verified
  across Rust, TypeScript, and Go with no per-language churn. Calls (`useState(`,
  `Command::new(`) and Python `def foo(` are unchanged.
- **Language inference no longer flips on a stray asset file.** When no `--type`
  was given, `infer_lang_from_path` counted extensions and took the mode via a
  non-deterministic `HashMap` max — so a single `report.html` beside `main.rs`
  could make a Rust directory infer as HTML. The choice is now deterministic and
  **prefers a real programming language over markup/style (html/css)**, breaking
  remaining ties alphabetically.
- Both behaviours are covered by `cargo test` (bare-signature translation incl.
  modifiers like `pub async fn`, call-form preservation, Python `def`, and the
  programming-beats-markup / tie-break inference rules).

### Known follow-ups

- `fn foo` (bare) won't match `pub fn foo` if the user omits the modifier the
  source uses — an inherent limit of matching by literal signature prefix.

## [0.3.7] - 2026-06-06

### Changed — flag-agnostic argument parsing (ends the "add more rg flags" churn)

- **Replaced the clap-derive flag struct with a purpose-built extractor**
  (`parse_rg_invocation`). The old struct had to enumerate ripgrep's ~150 flags;
  any flag it didn't know made `clap` abort the whole parse, the pattern was never
  seen, and the call fell to a lossy `clap_unparsed` fallback (≈67% of all calls).
  The new parser reads only what the shim needs — pattern, search path, `--type`,
  and the `-c`/`-l` output modes — and **treats every unrecognised flag as an
  opaque, harmless token**. A future ripgrep flag can no longer derail a call.
  The only enumeration kept is "which flags take a value" (~30 stable entries);
  an omission is non-fatal (it can mislabel a logged pattern, never change the
  user's actual search, which always forwards the original args verbatim).
- Covered by unit tests (`cargo test`) for the Claude Code canonical call shape,
  `-e`/`--regexp`, `--flag=value`, bundled short flags, `--`, and the key
  invariant that an **unknown flag is treated as boolean, not an abort**.
- `smart-rg --version` now reports the shim's own version (was forwarding to real
  ripgrep because clap's `--version` returned `Err` from `try_parse_from`).

### Result

Verified end-to-end: previously-failing invocations (`--no-ignore --sort path
--no-heading --color never -g '!.git' …`, `--stats --column --no-messages …`,
`--pcre2 -e … --max-columns …`) now classify and redirect instead of being lost;
`clap_unparsed` for new calls is **0**.

## [0.3.6] - 2026-06-06

### Fixed — the "fixes only last momentarily" conceptual bug

The report's headline numbers were structurally pinned at zero/negative, so every
prior fix to capture/classify/parsing was real but **invisible** — the gauge it fed
could not move. Three root causes, diagnosed by walking the whole pipeline:

- **The savings metric was unmeasurable by construction.** "Files saved" assumed
  ast-grep reads *fewer files* than ripgrep — but both walk the same tree, so the
  figure is always ~0. Reframed the report around what the shim actually delivers:
  **precision** — `total_false_positives_avoided` (= `max(0, rg_results − ag_matches)`,
  the comment/string/partial hits a naive text search surfaces that ast-grep's
  structural match skips). Token/cost are kept as a secondary, clamped estimate.
  No schema migration: the metric is derived from columns already stored.

- **`log_comparison` was gated behind `count > 0`**, silently dropping ~83% of
  structural redirects (24 redirects → only 4 comparison rows) from the report.
  Every structural redirect is now recorded, including zero-match ones (a zero-match
  ast-grep result is itself precision data).

- **`estimated_cost_saved_cents` could go negative** and render as red "loss" cells.
  Clamped at 0 both at write time and in the aggregate (so legacy negative rows
  also render honestly).

- **Version drift** — `report.html` hardcoded `v0.3.4` while the binary was `0.3.5`,
  so a fresh build always *looked* un-deployed. The clap version attribute and the
  report now both derive from `CARGO_PKG_VERSION` (injected via a `__SHIM_VERSION__`
  placeholder). The report's detail table also no longer prefixes every value with a
  literal `−`, and adds a **Noise Avoided** column.

### Known follow-ups

- ~~clap-derive rejects unenumerated rg flags~~ → **fixed in 0.3.7** (flag-agnostic
  parser).
- ~~`smart-rg --version` forwards to real ripgrep~~ → **fixed in 0.3.7**.
- ast-grep can under-match some translated patterns (e.g. `fn main($$$)`), which
  inflates "noise avoided"; the pattern translator deserves a separate pass. *(Still
  open — when `--type` is absent, language inference can also pick the wrong language
  for a mixed-extension directory.)*

## [0.3.5] - 2026-06-05

### Fixed

- **Added 30+ missing rg flags to the clap argument parser.** Claude Code calls
  `rg` with flags like `--no-ignore`, `--sort`, `--no-heading`, `--color`,
  `-H/--with-filename`, `-w`, `-F`, `-e/--regexp`, `-m/--max-count`, and others
  that the old clap struct didn't recognize. Any unrecognized flag caused clap to
  bail, the shim fell to the `clap_unparsed` path, and the pattern was never
  extracted or classified. These flags now parse correctly, recovering ~393 events
  per session that were previously lost as opaque passthroughs.

- **`-e/--regexp` is now a first-class pattern flag.** When rg is called as
  `rg -e 'pattern' --type ts .` the pattern is now correctly extracted from the
  `-e` value rather than missed entirely.

- **Classifier now recognises full function signatures.** Patterns with a closing
  paren (e.g. `fn main($$$)`, `Command::new($$$)`) were rejected by the
  function-call branch because only `pattern.ends_with('(')` was accepted. Any
  pattern containing matching parens is now classified as structural.

- **Improved fallback pattern extraction when clap still fails.** The previous
  fallback picked the first non-flag argument, which was often a glob value
  (`!.git`), a context count (`4`), or a path — not the actual search term. The
  new extractor checks for `-e/--regexp` first, then skips known flag-value
  pairs and path-looking positionals before selecting the real pattern.

- **Language inference from path when no `--type` flag is given.** When a
  structural pattern is found but no `--type/-t` flag was passed (a very common
  Claude Code call shape), the shim now scans the search path (max depth 2,
  skipping `node_modules`/`target`) and picks the dominant file-extension language.
  This recovers `no_language` passthroughs for most real-project searches.

## [0.3.4] - 2026-06-01

### Fixed

- **ast-grep "no matches" is no longer logged as an error.** ast-grep exits `1`
  when a pattern matches nothing — the normal empty-result case. The runner
  treated any non-zero exit as a failure, so every empty structural search was
  recorded as an `ast_grep_error` (and produced a duplicate `structural/0` event).
  A real failure is now distinguished by **non-empty stderr** (e.g. a bad path /
  unreadable stream); only those are logged as errors.
- **Genuine ast-grep failures now fall back to real ripgrep.** Previously a real
  error (e.g. `stream: No such file or directory`) was logged and then returned a
  silent empty result. The runner now forwards to real `rg` so the user always
  gets results.
- **Unparseable rg invocations are counted as passthroughs, not errors.** When
  `clap` cannot parse an `rg` flag combination the shim still forwards to real rg,
  so the event is a `passthrough` (`clap_unparsed`), not a `parse_error`. This
  stops ordinary regex searches (alternations, char classes, paths) from
  inflating the report's error count.

### Note

- The HTML report is built **only** from real intercepted searches. If an older
  `~/.smart-rg/stats.db` contains seeded benchmark `comparisons` rows, wipe them
  with `smart-rg reset --yes` (or delete just those rows) so the report reflects
  actual usage.

## [0.3.3] - 2026-05-31

### Changed

- **`smart-rg --help` now shows smart-rg's own help** — its subcommands
  (`stats`, `report`, `prune`, `reset`), drop-in search usage, and install
  management — instead of forwarding to ripgrep's multi-page `--help`. A bare
  `smart-rg` and `smart-rg help` show the same help. When the binary is invoked
  as `rg` (impersonating ripgrep), `rg --help` still forwards to the real
  ripgrep, so the `rg` contract is unchanged. Help text credits ripgrep and
  ast-grep and points power users to `rg --help` for the full flag list.

## [0.3.2] - 2026-05-31

### Fixed

- **Upgrades now clean up shim artifacts from *any* prior install location.**
  `migrate_old_shim` previously probed only a couple of fixed paths
  (`/usr/local/bin/rg`, `~/.local/bin/rg`). But older installers' PATH-fix could
  drop a `rg` symlink into any user-writable dir ahead of Homebrew (e.g. `~/bin`),
  leaving an orphan after upgrade. It now scans every PATH dir plus the
  well-known legacy spots and removes anything outside the dedicated bin that is
  unmistakably ours (symlink → smart-rg, or the `smart-rg:` binary signature).
- **`self-verify` no longer falsely FAILs when your shell startup prints output.**
  The probe captured the shell's entire `-c` output, so any banner from
  oh-my-zsh / powerlevel10k / MOTD / session-restore contaminated the result and
  the verify reported FAIL even though `rg` resolved correctly. It now extracts
  just the resolved `rg` path.

### Docs

- README rewritten for the v0.3 dedicated-bin model: no `sudo` / `/usr/local/bin`,
  the `~/.smart-rg/bin` + `env.sh` PATH drop-in, the `smart-rg` command, the
  `--uninstall`/`--purge` flow, and removal of the obsolete `--with-grep` /
  `--no-fix-path` / manifest references.

## [0.3.1] - 2026-05-31

### Fixed

- **`smart-rg` command not found after install.** The dedicated-bin model
  installed only `rg`, but the installer's hints (and the docs) reference
  `smart-rg stats` / `smart-rg report`. The installer now also creates a
  `smart-rg` command in `~/.smart-rg/bin` (a relative symlink to the same
  binary, which routes subcommands by argv), and `--uninstall` removes it.

## [0.3.0] - 2026-05-30

### Fixed

- **(B) Potential infinite re-exec loop / fork bomb on Linux.** The shim used to
  find the real ripgrep by checking a couple of hardcoded paths and then falling
  back to a bare `rg` lookup on `PATH`. With the new installer putting the shim's
  own directory first on `PATH`, that bare lookup resolves straight back to the
  shim, which re-execs itself forever. The shim now resolves the real ripgrep via
  `~/.smart-rg/bin/rg2` (a symlink the installer points at the genuine binary)
  with self-exclusion — it otherwise scans `PATH` only for an `rg` whose canonical
  path is neither this executable nor inside `~/.smart-rg/bin`, and never falls
  back to a bare `rg`. If no real ripgrep is found it prints a clear error and
  exits non-zero instead of looping.
- **Installer resolves the real ripgrep by content, not by path string.** A stale
  shim left at `/opt/homebrew/bin/rg` (probed first) could previously be selected
  as "real rg" and loop. `resolve_real_rg` now skips any candidate detected as our
  shim — by symlink target, the dedicated path, or the binary's `smart-rg:`
  signature — wherever it lives.
- **ROI baseline was systematically wrong.** The rg comparison replayed the raw
  structural pattern (e.g. `foo(`), which is an invalid regex, so the baseline
  silently collapsed to 0; and it appended the search path even when the args
  already carried it, double-counting every file. The baseline now matches
  literally (`-F`) and appends the path only when absent.
- **Report figures were inconsistent.** Headline KPI totals now fall back to the
  real `text − ast` token/cost figures exactly like the per-row table (they could
  disagree before); the per-row "Net Saved" is shown in cents (was 100× too small);
  and `comparisons.estimated_cost_saved_cents` is stored as `REAL`, not `INTEGER`.
- Comparison rows now record the **raw user pattern** (not the translated
  ast-grep form) so the report's Pattern column matches the numbers beside it.
- `--type` baseline filtering now globs **every** language the shim recognizes
  (ripgrep has no `tsx`/`jsx` type and names Rust/Ruby `rust`/`ruby`, so the old
  pass-through errored those to a 0 baseline).

### Changed

- **Durable PATH interception via a dedicated `~/.smart-rg/bin`.** The shim lives
  in its own directory forced to the front of `PATH` through a drop-in
  (`~/.smart-rg/env.sh`) sourced from a marked block in each shell startup file;
  the real ripgrep is symlinked to `~/.smart-rg/bin/rg2`. Install is idempotent
  (legacy/duplicate blocks are stripped first) and `--uninstall` leaves no orphans.
