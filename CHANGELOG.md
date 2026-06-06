# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
