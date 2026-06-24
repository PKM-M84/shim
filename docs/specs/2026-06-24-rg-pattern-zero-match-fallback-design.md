# Design: zero-match rg fallback + `rg PATTERN -` stdin fix

Date: 2026-06-24
Status: approved (Chris, 2026-06-24)
Affects: `src/main.rs`, `Cargo.toml`, `Cargo.lock`

## Problem

Two silent-empty misroutes remain in the shim.

### 1. Bare-identifier / wrong-language misroute (the live bug)

`classify()` marks any bare identifier with mixed case or an underscore as
*structural*, so `rg frameworkReplaceCommand` (no path) is redirected to
ast-grep. `main()` then infers a **single** dominant language from the cwd
(`infer_lang_from_path(".")` → `dominant_lang`). In a polyglot repo the symbol
often lives in a different language than the guess, so ast-grep runs
`--lang <wrong>`, skips the file, and exits with **0 matches and empty stderr**.
The caller reads a phantom empty result as a real no-match.

Confirmed in the live event log (`~/.smart-rg/stats.db`) from agentvault-gen2
sessions: `frameworkReplaceCommand` logged as both `javascript` and `python`
seconds apart (two wrong guesses, both 0); `policy_restricted` searched ~8×
across js/python, always 0; likewise `safeHermesProfile`, `attachLifecycle`,
`buildPluginCommand`, `terminalReenrollMessage`.

This is the same class as the `--files` path-hijack (v0.3.9) and the piped-stdin
bug (v0.3.10): `classify()` decides on the pattern alone and the redirect can
land ast-grep on the wrong files. The wrong-language guess affects *structural*
patterns too (`rg 'console.log('` returns 0 if the dominant guess is python).

### 2. `rg PATTERN -` explicit-stdin edge case (parked)

A positional `-` means "read stdin". `parse_rg_invocation` treats it as a path,
so `has_path` becomes true and the call dodges the v0.3.10 stream-filter guard
(`is_stream_filter` = no-path + non-TTY). ast-grep cannot search `-`, so the
redirect produces a broken/empty search.

## Decision

Reverse decision #9 (previously: no fallback on empty). It is now acceptable
**because the telemetry stays honest** (see below) and the silent-empty failure
is far more damaging than a little fallback noise. Chris accepted the one cost:
a genuinely-empty *structural* search may now surface a few comment/string lines
from rg instead of clean nothing.

## Approach

### A. Zero-match fallback to real rg

After a structural redirect, if ast-grep exits with **0 matches AND empty
stderr**, re-run real rg with the **original args** and return its output and
exit code.

- Genuine ast-grep *errors* (non-empty stderr) already fall back to rg
  (v0.3.4 behavior) — unchanged.
- ast-grep prints nothing on 0 matches, so the subsequent rg run does not
  double-print.
- The fallback uses the original argv verbatim, so the user's real search
  (pattern, flags, paths) runs exactly as typed.

### B. Honest telemetry — the mitigation that makes A safe

A 0-match-then-fallback is logged as a new event type **`fallback`**, never as a
`structural` win. It does not count toward "noise matches avoided". Report KPIs
that sum `structural` rows are unaffected, so the headline metric cannot be
inflated by misroutes-turned-fallbacks. This preserves the spirit of decision #9
(no fake savings) while removing the silent-empty failure.

Event semantics after this change:
- `structural` — redirected to ast-grep, **matches > 0**. The only "win".
- `fallback`  — redirected, ast-grep returned 0; real rg was run instead. NEW.
- `passthrough` — never redirected (not structural / no language / stream).

Note: structural redirects that legitimately return 0 (pattern truly absent in
the right language) will now also be logged `fallback` rather than
`structural`+0. That is acceptable and arguably more accurate — a 0-match
redirect was never a real noise-avoided win.

### C. `rg PATTERN -` stdin fix

In `parse_rg_invocation`, a positional equal to `-` is recognised as stdin: it
is **not** recorded as a path (`has_path` stays false for it). The call then
falls through `is_stream_filter` (no real path) → verbatim passthrough to rg.
An explicit real path alongside (`rg PATTERN - src/`) still sets `has_path`.

## Net behavior for the live bug

`rg frameworkReplaceCommand` → classified structural → ast-grep `--lang <guess>`
→ 0 matches, empty stderr → **fall back to real rg → real hits returned**,
logged as `fallback`. No more silent empties.

## Testing (TDD, red first)

Unit tests in `src/main.rs` (extends the existing 31-test suite):

1. `parse_rg_invocation(["PATTERN", "-"])` → pattern = "PATTERN", `has_path`
   false (dash is stdin, not a path).
2. `parse_rg_invocation(["PATTERN", "-", "src/"])` → `has_path` true, path
   "src/" (real path wins; dash ignored as stdin marker).
3. `is_stream_filter` unchanged; combined with (1) a `PATTERN -` call routes to
   passthrough.
4. A fallback-decision helper (pure function over `match_count` + `stderr_empty`)
   returns: redirect-win when matches>0; fall-back when matches==0 &&
   stderr_empty; error-fallback when stderr non-empty.

Integration smoke (manual, against agentvault-gen2 after install):
- `rg frameworkReplaceCommand` returns the real `.ts` hits (was 0).
- A unique-absent token returns clean empty (rg also 0 → no noise).

## Out of scope

- Multi-language ast-grep redirect (Approach 2) — not pursued; fallback covers
  the wrong-language case more simply.
- User-configurable cost pricing (parked, separate brainstorm).
- Changing `classify()` heuristics — left as-is; the fallback is the safety net.

## Release

Bump `Cargo.toml` minor/patch to 0.3.12; `cargo build` to sync `Cargo.lock` in
the same commit (lesson from PR #7/#9). Ship via branch → PR → tag-triggered
auto-release (now idempotent, PR #8). Verify live on Chris's mac.
