# Zero-Match rg Fallback + `rg PATTERN -` Stdin Fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rg PATTERN` for a bare identifier (and any redirect that ast-grep returns empty for) fall back to real rg instead of silently returning nothing, and route `rg PATTERN -` (explicit stdin) to passthrough.

**Architecture:** Three surgical changes in `src/main.rs`: (1) `parse_rg_invocation` recognises a positional `-` as stdin (`reads_stdin`) and never counts it as a real path; (2) `main` passes through when `reads_stdin`; (3) on a structural redirect where ast-grep returns 0 matches with empty stderr, `main` falls back to real rg and logs a new `fallback` event, while `run_ast_grep` gates its savings/comparison row on `count > 0`.

**Tech Stack:** Rust, `cargo test`, SQLite (rusqlite), ast-grep CLI.

Spec: `docs/specs/2026-06-24-rg-pattern-zero-match-fallback-design.md`
Issue: PKM-M84/shim#12

---

## File Structure

- Modify: `src/main.rs`
  - `RgInvocation` struct (~line 130-156): add `reads_stdin: bool`.
  - `parse_rg_invocation` (~line 281-285): set `reads_stdin`; never treat `-` as a real path.
  - `main` (~line 442, 464-472): passthrough on `reads_stdin`; fallback on 0-match redirect.
  - new pure helper `redirect_outcome` near `is_stream_filter` (~line 296): decide win vs fallback.
  - `run_ast_grep` (~line 1045-1051): gate the `log_comparison` block on `count > 0`.
  - `#[cfg(test)] mod tests` (~line 1600+): new unit tests.
- Modify: `Cargo.toml` — version `0.3.11` → `0.3.12`.
- Modify: `Cargo.lock` — synced by `cargo build` in the same commit.

No new files. Follows the existing single-file structure and the existing
`mod tests` pure-function test style (`parse()` helper).

---

### Task 1: Recognise `-` as stdin in the parser

**Files:**
- Modify: `src/main.rs` (RgInvocation struct; `parse_rg_invocation` path assignment)
- Test: `src/main.rs` `mod tests`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` (after the existing `has_path_set_only_when_a_positional_path_is_given` test):

```rust
    #[test]
    fn dash_positional_is_stdin_not_a_path() {
        let inv = parse(&["PATTERN", "-"]);
        assert_eq!(inv.pattern.as_deref(), Some("PATTERN"));
        assert!(inv.reads_stdin, "trailing - marks explicit stdin");
        assert!(!inv.has_path, "- is stdin, not a real path");
    }

    #[test]
    fn dash_plus_real_path_keeps_the_real_path() {
        let inv = parse(&["PATTERN", "-", "src/"]);
        assert!(inv.reads_stdin, "- still marks stdin");
        assert!(inv.has_path, "src/ is a real path");
        assert_eq!(inv.path, "src/");
    }

    #[test]
    fn no_dash_means_no_stdin() {
        let inv = parse(&["foo(", "./src"]);
        assert!(!inv.reads_stdin);
        assert!(inv.has_path);
        assert_eq!(inv.path, "./src");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test dash_ 2>&1 | tail -20`
Expected: compile error — `no field reads_stdin on type RgInvocation`.

- [ ] **Step 3: Add the field to the struct**

In the `RgInvocation` struct (the block ending at line ~156, right after `has_path: bool,`), add:

```rust
    // A positional `-` (ripgrep's explicit stdin marker). ast-grep cannot read
    // stdin, so any call that reads stdin must pass through to real rg — even
    // when stdin is a TTY (the user asked for it explicitly).
    reads_stdin: bool,
```

- [ ] **Step 4: Set it in `parse_rg_invocation` and exclude `-` from real paths**

Replace the path-assignment block (currently lines ~281-284):

```rust
    if let Some(p) = paths.first() {
        inv.path = p.clone();
        inv.has_path = true;
    }
```

with:

```rust
    // A positional `-` is stdin, not a path. Record it so main forwards the
    // call, and pick the first NON-dash positional as the real search path.
    inv.reads_stdin = paths.iter().any(|p| p == "-");
    if let Some(p) = paths.iter().find(|p| p.as_str() != "-") {
        inv.path = p.clone();
        inv.has_path = true;
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test dash_ no_dash_ 2>&1 | tail -20`
Expected: 3 passed.

- [ ] **Step 6: Run the full suite (no regressions)**

Run: `cargo test 2>&1 | tail -15`
Expected: all existing tests + 3 new pass.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "fix: treat positional - as stdin, not a path (rg PATTERN -)

Refs PKM-M84/shim#12

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Pass through when the call reads stdin

**Files:**
- Modify: `src/main.rs` (`main`, the stream-filter guard ~line 442)

- [ ] **Step 1: Update the stream-filter guard in `main`**

Replace (currently lines ~442-445):

```rust
    if is_stream_filter(inv.has_path, std::io::stdin().is_terminal()) {
        log_event("passthrough", &pattern, "stream_stdin", None, 0);
        exec_real_rg(&args[1..]);
    }
```

with:

```rust
    // Explicit `-` stdin OR an implicit pipe (no path + non-TTY stdin): ast-grep
    // has no stdin-search mode, so forward verbatim or the stream is dropped.
    if inv.reads_stdin || is_stream_filter(inv.has_path, std::io::stdin().is_terminal()) {
        let reason = if inv.reads_stdin { "stdin_dash" } else { "stream_stdin" };
        log_event("passthrough", &pattern, reason, None, 0);
        exec_real_rg(&args[1..]);
    }
```

- [ ] **Step 2: Build (no test — process-level behavior)**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles clean (no warnings about unused `reads_stdin`).

- [ ] **Step 3: Manual smoke — `-` forces passthrough even from a TTY**

Run: `printf 'frameworkReplaceCommand\n' | ./target/debug/smart-rg PATTERN -`
Expected: behaves like rg reading stdin (the line is filtered against `PATTERN`, here no match → empty, exit 1), NOT a `🔀 smart-rg → ast-grep` redirect line on stderr.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "fix: pass through to rg when the call reads stdin

Refs PKM-M84/shim#12

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Fallback decision helper (pure, tested)

**Files:**
- Modify: `src/main.rs` (new `RedirectOutcome` enum + `redirect_outcome` fn near `is_stream_filter`)
- Test: `src/main.rs` `mod tests`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn redirect_outcome_win_when_matches_and_no_stderr() {
        assert_eq!(redirect_outcome(3, true), RedirectOutcome::Win);
    }

    #[test]
    fn redirect_outcome_fallback_empty_when_zero_and_no_stderr() {
        assert_eq!(redirect_outcome(0, true), RedirectOutcome::FallbackEmpty);
    }

    #[test]
    fn redirect_outcome_fallback_error_when_stderr_present() {
        // A genuine ast-grep error falls back regardless of count.
        assert_eq!(redirect_outcome(0, false), RedirectOutcome::FallbackError);
        assert_eq!(redirect_outcome(5, false), RedirectOutcome::FallbackError);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test redirect_outcome 2>&1 | tail -20`
Expected: compile error — `cannot find function redirect_outcome` / `RedirectOutcome`.

- [ ] **Step 3: Implement the enum + helper**

Insert directly after the `is_stream_filter` function (after line ~296):

```rust
/// What to do after attempting an ast-grep redirect.
#[derive(Debug, PartialEq, Eq)]
enum RedirectOutcome {
    /// ast-grep found matches and printed them — count it as a real win.
    Win,
    /// ast-grep ran cleanly but found nothing. Fall back to real rg so a
    /// wrong-language guess (or any blind spot) can't return a silent empty.
    FallbackEmpty,
    /// ast-grep wrote to stderr — a genuine error. Fall back to real rg.
    FallbackError,
}

/// Decide the outcome from ast-grep's match count and whether stderr was empty.
/// A non-empty stderr is a real error and always falls back; otherwise zero
/// matches falls back and any matches is a win.
fn redirect_outcome(match_count: u64, stderr_empty: bool) -> RedirectOutcome {
    if !stderr_empty {
        RedirectOutcome::FallbackError
    } else if match_count == 0 {
        RedirectOutcome::FallbackEmpty
    } else {
        RedirectOutcome::Win
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test redirect_outcome 2>&1 | tail -20`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: redirect_outcome helper (win / fallback-empty / fallback-error)

Refs PKM-M84/shim#12

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Gate the savings comparison row on count > 0

**Files:**
- Modify: `src/main.rs` (`run_ast_grep`, the comparison block ~lines 1045-1051)

- [ ] **Step 1: Wrap the comparison logging in a `count > 0` guard**

Replace the block (currently lines ~1045-1051):

```rust
    {
        let raw_pattern = inv.pattern.as_deref().unwrap_or(sg_pattern);
        let rg_start = Instant::now();
        let (rg_results, rg_file_count) = run_rg_count(&std::env::args().skip(1).collect::<Vec<_>>(), path);
        let rg_time_ms = rg_start.elapsed().as_millis() as u64;
        log_comparison(raw_pattern, lang, count, ag_file_count, ag_time_ms, rg_results, rg_file_count, rg_time_ms);
    }
```

with:

```rust
    // Only credit savings when ast-grep actually won (count > 0). On a 0-match
    // redirect main now falls back to real rg and SHOWS those results, so
    // crediting "noise avoided" for them would be false. (Supersedes the earlier
    // v0.3.6 "log every redirect incl. count==0" choice — see issue #12.)
    if count > 0 {
        let raw_pattern = inv.pattern.as_deref().unwrap_or(sg_pattern);
        let rg_start = Instant::now();
        let (rg_results, rg_file_count) = run_rg_count(&std::env::args().skip(1).collect::<Vec<_>>(), path);
        let rg_time_ms = rg_start.elapsed().as_millis() as u64;
        log_comparison(raw_pattern, lang, count, ag_file_count, ag_time_ms, rg_results, rg_file_count, rg_time_ms);
    }
```

- [ ] **Step 2: Build**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles clean.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "fix: only log savings comparison when ast-grep found matches

Refs PKM-M84/shim#12

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Wire the fallback into `main`

**Files:**
- Modify: `src/main.rs` (`main`, lines ~464-472)

- [ ] **Step 1: Replace the redirect-logging + exit block**

Replace (currently lines ~464-472):

```rust
    let match_count = run_ast_grep(&sg_pattern, lang, &inv.path, &inv);

    // Log the successful redirect
    log_event("structural", &sg_pattern, "redirected", Some(lang), match_count);

    if match_count == 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::process::exit(1);
    }
}
```

with:

```rust
    let match_count = run_ast_grep(&sg_pattern, lang, &inv.path, &inv);

    // run_ast_grep already handled the FallbackError case (non-empty stderr ->
    // exec real rg). Here we only see clean runs, so empty stderr is implied.
    match redirect_outcome(match_count, true) {
        RedirectOutcome::Win => {
            log_event("structural", &sg_pattern, "redirected", Some(lang), match_count);
        }
        RedirectOutcome::FallbackEmpty | RedirectOutcome::FallbackError => {
            // ast-grep found nothing (often a wrong-language guess over a
            // polyglot tree). Fall back to real rg so the user gets real hits
            // instead of a silent empty. Logged as `fallback`, never a
            // structural win, so the report's noise-avoided metric stays honest.
            log_event("fallback", &pattern, "ast_grep_empty", Some(lang), 0);
            exec_real_rg(&args[1..]);
        }
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles clean.

- [ ] **Step 3: Run the full suite**

Run: `cargo test 2>&1 | tail -15`
Expected: all tests pass (34 total: 31 prior + 3 dash + 3 outcome − none removed = 37; confirm count climbs, all green).

- [ ] **Step 4: Manual smoke — the live bug, reproduced and fixed**

Set up a tiny polyglot fixture where the symbol lives in TS but JS/py would be guessed:

```bash
tmp=$(mktemp -d)
mkdir -p "$tmp/a" "$tmp/b"
printf 'export function frameworkReplaceCommand() {}\n' > "$tmp/a/cmd.ts"
# pad with more .js files so dominant_lang could guess javascript
printf 'const x=1;\n' > "$tmp/b/one.js"; printf 'const y=2;\n' > "$tmp/b/two.js"
( cd "$tmp" && /Users/user/Documents/Projects/sandbox/smart-rg-shim/target/debug/smart-rg frameworkReplaceCommand )
echo "exit=$?"
rm -rf "$tmp"
```

Expected: the `.ts` line is printed (via ast-grep if ts guessed, or via the rg
fallback if js guessed) — **never a silent empty**. exit=0.

- [ ] **Step 5: Manual smoke — genuinely-absent token is clean empty**

```bash
tmp=$(mktemp -d); printf 'export function realThing() {}\n' > "$tmp/x.ts"
( cd "$tmp" && /Users/user/Documents/Projects/sandbox/smart-rg-shim/target/debug/smart-rg zzNoSuchSymbolzz ); echo "exit=$?"
rm -rf "$tmp"
```

Expected: no output, exit=1 (rg's no-match code via fallback). No noise.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: fall back to real rg on a 0-match structural redirect

Reverses decision #9: a 0-match ast-grep redirect (commonly a wrong-language
guess over a polyglot tree) now re-runs real rg instead of returning a silent
empty. Logged as a new 'fallback' event so the noise-avoided metric stays honest.

Closes PKM-M84/shim#12

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Version bump + lockfile sync

**Files:**
- Modify: `Cargo.toml` (version), `Cargo.lock` (synced)

- [ ] **Step 1: Bump the version**

In `Cargo.toml`, change `version = "0.3.11"` to `version = "0.3.12"`.

- [ ] **Step 2: Sync the lockfile (same commit — lesson from PR #7/#9)**

Run: `cargo build 2>&1 | tail -3`
Then confirm: `grep -A1 'name = "smart-rg"' Cargo.lock | grep version`
Expected: `version = "0.3.12"`.

- [ ] **Step 3: Verify the shim reports the new version**

Run: `./target/debug/smart-rg --version`
Expected: `smart-rg 0.3.12`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to 0.3.12

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Approach A (zero-match fallback) → Tasks 3 + 5. ✓
- Approach B (honest telemetry: `fallback` event, gated comparison) → Task 4 (savings gate) + Task 5 (`fallback` event). ✓
- Approach C (`rg PATTERN -` stdin) → Tasks 1 + 2. ✓
- Existing v0.3.4 error-fallback preserved → unchanged in `run_ast_grep`; `redirect_outcome` documents it (FallbackError). ✓
- Release/version/lockfile → Task 6. ✓

**Placeholder scan:** none — every code step shows full code; every run step shows the command + expected output.

**Type consistency:** `reads_stdin: bool` defined Task 1, used Task 2. `RedirectOutcome`/`redirect_outcome(u64, bool)` defined Task 3, used Task 5. `log_event("fallback", ...)` signature matches existing `log_event(&str,&str,&str,Option<&str>,u64)`. `count > 0` gate (Task 4) aligns with `redirect_outcome` FallbackEmpty (Task 5). ✓

**Note on test count:** the suite was reported as 31 tests in memory; this plan adds 6. State the observed pre-count by running `cargo test 2>&1 | grep 'test result'` before Task 1 if exact numbers matter; the gate is "all green," not a specific total.
