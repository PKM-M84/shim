# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
