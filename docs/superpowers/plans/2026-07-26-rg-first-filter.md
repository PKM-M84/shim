# rg-first, ast-grep-as-filter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Invert the shim's pipeline so ripgrep runs first as ground truth and ast-grep filters its hits, eliminating wrong-language misses and the 249 double-run fallbacks.

**Architecture:** Today: `classify → ast-grep → (empty?) → rg`. After: `classify → rg --json (captured) → group matched files by language → ast-grep per language over just those files → keep rg hits contained in a confirmed node span`. Language is derived from the files ripgrep actually matched rather than guessed from a filesystem walk. A search ripgrep cannot find never reaches ast-grep.

**Tech Stack:** Rust 2021, `clap` 4 (subcommands only), `rusqlite` 0.31 (bundled), `serde_json` 1. External binaries: `rg` (ripgrep), `ast-grep`.

**Spec:** `docs/specs/2026-07-26-rg-first-filter-design.md`

## Global Constraints

- All code lives in `src/main.rs` (single-file binary — the established convention). Do **not** split the file in this plan; it is ~2,265 lines and a split is a separate, follow-up change.
- Every task is TDD: write the failing test, run it, watch it fail for the right reason, then implement.
- Run the full suite with `cargo test` (57 tests exist and must stay green).
- Never re-exec ripgrep once it has already run — that reintroduces the double-run this work removes.
- ast-grep's JSON `range.start.line` and `range.end.line` are **0-indexed**; ripgrep's `line_number` is **1-based**. Normalise ast-grep to 1-based at parse time.
- Preserve the v0.3.13 unsupported-flag deny-list behaviour exactly: those calls passthrough before any of this runs. The nine-flag matrix in Task 8 must stay byte-identical to real rg.
- Suppressed-match notices go to **stderr**, never stdout.
- Match cap: `10_000`.

---

### Task 1: `classify()` accepts escaped literals

Recovers the 29 wrongly-rejected call searches (`store_mls_message\(`). `classify()` currently returns false on any backslash, even though `translate_pattern` already strips backslashes.

**Files:**
- Modify: `src/main.rs:789` (`classify`)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Produces: `fn literal_form(pattern: &str) -> Option<String>` — the pattern's literal text, or `None` when it carries real regex semantics.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    // ── classify: escaped punctuation is a literal, not a regex ──
    //
    // Agents write `store_mls_message\(` because a bare `(` is an invalid
    // regex for rg. classify() rejected every one of them on the backslash —
    // 29 wrongly-rejected call searches in the live event log.

    #[test]
    fn escaped_paren_call_is_structural() {
        assert!(classify(r"store_mls_message\("));
        assert!(classify(r"bypass_rls\("));
    }

    #[test]
    fn escaped_dot_is_structural() {
        assert!(classify(r"app\.current_tenant_id"));
    }

    #[test]
    fn regex_assertions_are_not_structural() {
        // \b is a word boundary, not an escaped literal `b`.
        assert!(!classify(r"\btext\("));
        assert!(!classify(r"\d+"));
    }

    #[test]
    fn escaped_alternation_is_still_a_regex() {
        assert!(!classify(r"a\(|b\("));
    }

    #[test]
    fn literal_form_unescapes_punctuation_only() {
        assert_eq!(literal_form(r"store_mls_message\("), Some("store_mls_message(".into()));
        assert_eq!(literal_form(r"\btext"), None, "word-boundary assertion");
        assert_eq!(literal_form(r"a|b"), None, "unescaped alternation");
        assert_eq!(literal_form(r"trailing\"), None, "dangling backslash");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test classify 2>&1 | tail -20`
Expected: compile error, `cannot find function 'literal_form' in this scope`.

- [ ] **Step 3: Implement `literal_form` and rewire `classify`**

Insert immediately above `fn classify` (`src/main.rs:789`):

```rust
/// Reduce a pattern to its literal text when its only regex syntax is escaped
/// punctuation, else None.
///
/// Agents escape parens (`store_mls_message\(`) because a bare `(` is an
/// invalid regex for ripgrep — that is the CANONICAL shape of a call search,
/// and rejecting it on the backslash threw away 29 real structural searches.
/// `\` followed by a letter or digit is an assertion or class (`\b`, `\d`,
/// `\w`, `\s`), which is real regex semantics, so the pattern is text.
fn literal_form(pattern: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(n) if n.is_alphanumeric() => return None, // \b \d \w \s …
                Some(n) => out.push(n),                        // escaped literal
                None => return None,                           // dangling backslash
            }
        } else {
            // Unescaped regex metacharacters mean a genuine text search.
            // NOTE: `(`, `)` and `.` are deliberately absent — classify()
            // treats them as structural indicators.
            if matches!(c, '|' | '[' | ']' | '*' | '+' | '?' | '{' | '}' | '^' | '$') {
                return None;
            }
            out.push(c);
        }
    }
    Some(out)
}
```

Then in `classify`, replace the opening backslash and slash guards:

```rust
fn classify(pattern: &str) -> bool {
    // Escaped-literal patterns reduce to their literal text; anything carrying
    // real regex semantics is a text search and passes through.
    let literal = match literal_form(pattern) {
        Some(l) => l,
        None => return false,
    };

    // Path-like tokens are never structural. A '/' cannot appear in an
    // identifier or call pattern in any supported language.
    if literal.contains('/') {
        return false;
    }

    let raw = literal.trim();
```

Delete the two old guards (`if pattern.contains('\\') { return false; }` and `if pattern.contains('/') { return false; }`) and the old `let raw = pattern.trim();`. Everything below `let raw` is unchanged.

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 62 tests. `regex_with_slash_alternation_is_not_structural` and `absolute_path_is_not_structural` must still pass — they now fail at `literal_form` (unescaped `*`/`|`) or the `/` check.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "fix: classify escaped-literal patterns as structural

Agents write store_mls_message\\( because a bare ( is an invalid regex for
rg. classify() bailed on any backslash, rejecting all 29 such call searches
in the live event log, even though translate_pattern already strips
backslashes. \\ followed by a letter stays a regex assertion (\\b, \\d)."
```

---

### Task 2: Parse ripgrep's `--json` output

**Files:**
- Modify: `src/main.rs` (add above `struct AgMatch`, `src/main.rs:1045`)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Produces: `struct RgMatch { file: String, line: u64, text: String }` and `fn parse_rg_json(stdout: &str) -> Vec<RgMatch>`. Tasks 3, 6 and 7 consume both.

- [ ] **Step 1: Write the failing tests**

```rust
    // ── ripgrep --json parsing ──

    fn rg_json_match(file: &str, line: u64, text: &str) -> String {
        format!(
            r#"{{"type":"match","data":{{"path":{{"text":"{file}"}},"lines":{{"text":"{text}\n"}},"line_number":{line},"absolute_offset":0,"submatches":[]}}}}"#
        )
    }

    #[test]
    fn parses_a_match_record_with_one_based_line() {
        let m = parse_rg_json(&rg_json_match("src/a.ts", 1, "const deviceId = 1;"));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].file, "src/a.ts");
        assert_eq!(m[0].line, 1, "ripgrep line_number is already 1-based");
        assert_eq!(m[0].text, "const deviceId = 1;", "trailing newline stripped");
    }

    #[test]
    fn ignores_non_match_records() {
        let stream = format!(
            "{}\n{}\n{}",
            r#"{"type":"begin","data":{"path":{"text":"src/a.ts"}}}"#,
            rg_json_match("src/a.ts", 3, "hit"),
            r#"{"type":"end","data":{"path":{"text":"src/a.ts"}}}"#
        );
        let m = parse_rg_json(&stream);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 3);
    }

    #[test]
    fn empty_stream_yields_no_matches() {
        assert!(parse_rg_json("").is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test parse_rg_json 2>&1 | tail -10`
Expected: compile error, `cannot find function 'parse_rg_json'`.

- [ ] **Step 3: Implement**

Insert above `struct AgMatch` (`src/main.rs:1045`):

```rust
/// One matching line as ripgrep reported it. Distinct from `AgMatch`: this is a
/// LINE ripgrep hit, whereas an `AgMatch` is a syntax NODE ast-grep confirmed.
#[derive(Debug, PartialEq, Clone)]
struct RgMatch {
    file: String,
    line: u64,
    text: String,
}

/// Parse ripgrep's `--json` event stream (one JSON object per line), keeping
/// only `match` records.
///
/// `--json` is used rather than `path:line:text` because a path may itself
/// contain a colon. Paths that are not valid UTF-8 arrive as a `bytes` field
/// instead of `text` and are skipped — they cannot be handed to ast-grep anyway.
fn parse_rg_json(stdout: &str) -> Vec<RgMatch> {
    stdout
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            if v.get("type")?.as_str()? != "match" {
                return None;
            }
            let d = v.get("data")?;
            Some(RgMatch {
                file: d.get("path")?.get("text")?.as_str()?.to_string(),
                line: d.get("line_number")?.as_u64()?,
                // ripgrep includes the trailing newline; strip it so rendering
                // controls line breaks.
                text: d.get("lines")?.get("text")?.as_str()?
                    .trim_end_matches('\n')
                    .to_string(),
            })
        })
        .collect()
}
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 65 tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: parse ripgrep --json into RgMatch"
```

---

### Task 3: Confirm rg hits by containment in ast-grep node spans

**Files:**
- Modify: `src/main.rs:1045` (`struct AgMatch`), `src/main.rs:1063` (`parse_ag_matches`)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Consumes: `RgMatch` (Task 2).
- Produces: `AgMatch` gains `end_line: u64`; `fn confirmed_spans(matches: &[AgMatch]) -> HashMap<&str, Vec<(u64, u64)>>` and `fn is_confirmed(hit: &RgMatch, spans: &HashMap<&str, Vec<(u64, u64)>>) -> bool`. Task 7 consumes both.

- [ ] **Step 1: Write the failing tests**

The existing `ag_json` helper in `mod tests` emits no `end` field — replace it with one that does, then add the tests:

```rust
    fn ag_json(file: &str, line: u64, text: &str) -> String {
        ag_json_span(file, line, line, text)
    }

    fn ag_json_span(file: &str, start: u64, end: u64, text: &str) -> String {
        format!(
            r#"{{"file":"{file}","range":{{"start":{{"line":{start}}},"end":{{"line":{end}}}}},"lines":"{text}"}}"#
        )
    }

    // ── containment, not equality ──
    //
    // A structural node can span several lines while ripgrep reports each
    // matching line separately:
    //
    //   result = bypass_rls(        <- rg hits; ast-grep node STARTS here
    //       session, tenant_id      <- rg ALSO hits here for `tenant_id`
    //   )
    //
    // Confirming on the start line alone silently drops the second hit — the
    // same silent-drop shape as v0.3.9, v0.3.10 and v0.3.12.

    #[test]
    fn ast_grep_end_line_is_normalised_to_one_based() {
        let m = parse_ag_matches(&ag_json_span("a.py", 0, 2, "bypass_rls("));
        assert_eq!(m[0].line, 1);
        assert_eq!(m[0].end_line, 3);
    }

    #[test]
    fn a_hit_on_a_continuation_line_is_confirmed() {
        let ag = parse_ag_matches(&ag_json_span("a.py", 0, 2, "bypass_rls("));
        let spans = confirmed_spans(&ag);
        let hit = RgMatch { file: "a.py".into(), line: 2, text: "  session, tenant_id".into() };
        assert!(is_confirmed(&hit, &spans), "line 2 is inside the node span 1..=3");
    }

    #[test]
    fn a_hit_outside_every_span_is_dropped() {
        let ag = parse_ag_matches(&ag_json_span("a.py", 0, 2, "bypass_rls("));
        let spans = confirmed_spans(&ag);
        let hit = RgMatch { file: "a.py".into(), line: 9, text: "# bypass_rls in a comment".into() };
        assert!(!is_confirmed(&hit, &spans));
    }

    #[test]
    fn a_hit_in_an_unconfirmed_file_is_dropped() {
        let ag = parse_ag_matches(&ag_json_span("a.py", 0, 2, "bypass_rls("));
        let spans = confirmed_spans(&ag);
        let hit = RgMatch { file: "other.sql".into(), line: 1, text: "-- bypass_rls".into() };
        assert!(!is_confirmed(&hit, &spans));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test confirmed 2>&1 | tail -10`
Expected: compile error, `no field 'end_line' on type 'AgMatch'` and `cannot find function 'confirmed_spans'`.

- [ ] **Step 3: Implement**

Add the field to `struct AgMatch` (`src/main.rs:1045`):

```rust
#[derive(Debug, PartialEq)]
struct AgMatch {
    file: String,
    line: u64,
    /// Last line of the node (1-based, inclusive). A node may span several
    /// lines; see `is_confirmed`.
    end_line: u64,
    text: String,
}
```

In `parse_ag_matches` (`src/main.rs:1063`), populate it alongside `line`:

```rust
                end_line: v
                    .get("range")
                    .and_then(|r| r.get("end"))
                    .and_then(|e| e.get("line"))
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0)
                    + 1,
```

Append after `parse_ag_matches`:

```rust
/// Line spans (1-based, inclusive) ast-grep confirmed, keyed by file.
fn confirmed_spans(matches: &[AgMatch]) -> HashMap<&str, Vec<(u64, u64)>> {
    let mut spans: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
    for m in matches {
        spans.entry(m.file.as_str()).or_default().push((m.line, m.end_line));
    }
    spans
}

/// Is this ripgrep hit inside a node ast-grep confirmed?
///
/// CONTAINMENT, not equality. ripgrep reports every matching line; ast-grep
/// reports one span per node. Comparing against the node's start line alone
/// silently drops hits on continuation lines of a multi-line call.
fn is_confirmed(hit: &RgMatch, spans: &HashMap<&str, Vec<(u64, u64)>>) -> bool {
    spans
        .get(hit.file.as_str())
        .is_some_and(|v| v.iter().any(|&(start, end)| hit.line >= start && hit.line <= end))
}
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 69 tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: confirm rg hits by containment in ast-grep node spans

A structural node can span several lines while rg reports each matching line
separately. Matching on the node start line alone would silently drop hits on
continuation lines."
```

---

### Task 4: `--no-smart` escape hatch

**Files:**
- Modify: `src/main.rs:143` (`struct RgInvocation`), `src/main.rs:230` (`parse_rg_invocation`)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Produces: `RgInvocation.no_smart: bool` and `fn strip_shim_flags(args: &[String]) -> Vec<String>`. Task 7 consumes both.

- [ ] **Step 1: Write the failing tests**

```rust
    // ── --no-smart: ours, not ripgrep's ──

    #[test]
    fn no_smart_is_detected_without_eating_the_pattern() {
        let inv = parse(&["--no-smart", "deviceId", "src"]);
        assert!(inv.no_smart);
        assert_eq!(inv.pattern.as_deref(), Some("deviceId"));
        assert_eq!(inv.path, "src");
    }

    #[test]
    fn no_smart_defaults_off() {
        assert!(!parse(&["deviceId", "src"]).no_smart);
    }

    #[test]
    fn strip_shim_flags_removes_only_our_flag() {
        let args: Vec<String> = ["-n", "--no-smart", "deviceId", "src"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(
            strip_shim_flags(&args),
            vec!["-n".to_string(), "deviceId".to_string(), "src".to_string()],
            "rg would reject --no-smart"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test no_smart 2>&1 | tail -10`
Expected: compile error, `no field 'no_smart' on type 'RgInvocation'`.

- [ ] **Step 3: Implement**

Add to `struct RgInvocation` (after `unsupported`, `src/main.rs:143`):

```rust
    /// `--no-smart`: force plain ripgrep, no structural filtering.
    no_smart: bool,
```

In `parse_rg_invocation`'s long-flag `match name` block, add an arm:

```rust
                "no-smart" => inv.no_smart = true,
```

Add beside `parse_rg_invocation`:

```rust
/// Remove flags that belong to the shim rather than ripgrep.
///
/// `--no-smart` is ours; forwarding it would make rg exit with a usage error.
/// The parser deliberately treats unknown flags as harmless booleans (right for
/// real rg flags, wrong for this one), so it needs removing explicitly.
fn strip_shim_flags(args: &[String]) -> Vec<String> {
    args.iter().filter(|a| a.as_str() != "--no-smart").cloned().collect()
}
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 72 tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: --no-smart flag, stripped before forwarding to rg"
```

---

### Task 5: Group matched files by language

Replaces guessing one dominant language from a filesystem walk — the polyglot miss where `arming_snapshot` was attempted as python, javascript *and* typescript, eleven empty runs.

**Files:**
- Modify: `src/main.rs` (add after `ext_to_lang`, `src/main.rs:763`)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Produces: `fn group_files_by_lang(files: &[String]) -> BTreeMap<&'static str, Vec<String>>`. Task 7 consumes it.

- [ ] **Step 1: Write the failing tests**

```rust
    // ── language comes from the files that MATCHED ──

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn polyglot_hits_split_into_one_group_per_language() {
        let groups = group_files_by_lang(&strs(&["svc/a.py", "web/b.ts", "svc/c.py"]));
        assert_eq!(groups.get("python"), Some(&strs(&["svc/a.py", "svc/c.py"])));
        assert_eq!(groups.get("typescript"), Some(&strs(&["web/b.ts"])));
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn files_with_no_known_language_are_dropped() {
        let groups = group_files_by_lang(&strs(&["README.md", "data.csv", "a.py"]));
        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("python"));
    }

    #[test]
    fn grouping_is_deterministic() {
        let groups = group_files_by_lang(&strs(&["b.ts", "a.py"]));
        let langs: Vec<&str> = groups.keys().copied().collect();
        assert_eq!(langs, vec!["python", "typescript"], "BTreeMap orders by language");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test group_files 2>&1 | tail -10`
Expected: compile error, `cannot find function 'group_files_by_lang'`.

- [ ] **Step 3: Implement**

Insert after `fn ext_to_lang` (`src/main.rs:763`):

```rust
/// Group the files ripgrep actually matched by language.
///
/// This replaces inferring ONE dominant language from a filesystem walk. On a
/// polyglot repo that guess is usually wrong: the live event log shows
/// `arming_snapshot` attempted as python, javascript and typescript — eleven
/// empty runs for one symbol. The files that matched cannot be wrong about
/// their own extension. BTreeMap keeps the ast-grep spawn order deterministic.
fn group_files_by_lang(files: &[String]) -> BTreeMap<&'static str, Vec<String>> {
    let mut groups: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for f in files {
        let ext = std::path::Path::new(f)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if let Some(lang) = ext_to_lang(ext) {
            groups.entry(lang).or_default().push(f.clone());
        }
    }
    groups
}
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 75 tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: group matched files by language instead of guessing one"
```

---

### Task 6: Capture ripgrep's output

**Files:**
- Modify: `src/main.rs` (add after `parse_rg_json`)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Consumes: `RgMatch`, `parse_rg_json` (Task 2); `real_rg_path` (`src/main.rs:43`).
- Produces: `const MATCH_CAP: usize`, `enum RgCapture { Matches(Vec<RgMatch>), OverCap(Vec<RgMatch>), Failed }`, `fn rg_capture_args(original: &[String]) -> Vec<String>`, `fn run_rg_capture(original: &[String]) -> RgCapture`. Task 7 consumes all.

- [ ] **Step 1: Write the failing tests**

Only the argument construction is unit-testable; the spawn is covered end-to-end in Task 8.

```rust
    // ── capturing rg: strip output-mode flags, keep filters ──

    #[test]
    fn rg_capture_args_strip_output_modes_and_add_json() {
        let args = strs(&["-c", "-n", "--heading", "-g", "*.ts", "deviceId", "src"]);
        let out = rg_capture_args(&args);
        assert!(out.contains(&"--json".to_string()));
        for stripped in ["-c", "-n", "--heading"] {
            assert!(!out.contains(&stripped.to_string()), "{stripped} must be stripped");
        }
        // Filters and positionals survive, in order.
        assert!(out.windows(2).any(|w| w == ["-g".to_string(), "*.ts".to_string()]));
        assert!(out.contains(&"deviceId".to_string()));
        assert!(out.contains(&"src".to_string()));
    }

    #[test]
    fn rg_capture_args_keep_the_type_filter() {
        let out = rg_capture_args(&strs(&["--type", "ts", "deviceId", "src"]));
        assert!(out.windows(2).any(|w| w == ["--type".to_string(), "ts".to_string()]));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test rg_capture_args 2>&1 | tail -10`
Expected: compile error, `cannot find function 'rg_capture_args'`.

- [ ] **Step 3: Implement**

Insert after `parse_rg_json`:

```rust
/// Above this many matches, stop filtering and render ripgrep's output as-is.
/// The shim already buffers ast-grep's JSON, so capturing is not new in kind —
/// but ripgrep matches more, so it is new in degree.
const MATCH_CAP: usize = 10_000;

enum RgCapture {
    /// Within the cap — eligible for structural filtering.
    Matches(Vec<RgMatch>),
    /// Too many to filter; render as-is. Note we still hold the matches:
    /// re-running ripgrep would reintroduce the double-run this design removes.
    OverCap(Vec<RgMatch>),
    /// ripgrep could not be run, or failed. Caller should passthrough.
    Failed,
}

/// Build the argv for the internal ripgrep run: the caller's FILTER flags are
/// kept (`-g`, `--type`, `-w`, the pattern, the paths) and OUTPUT-MODE flags are
/// dropped, because we render the shape ourselves from the filtered set.
///
/// Dropping `-n`/`-N` loses nothing: the caller's preference is already recorded
/// on `RgInvocation.line_numbers` and rendering reads it from there.
fn rg_capture_args(original: &[String]) -> Vec<String> {
    let mut out = vec!["--json".to_string()];
    for arg in original {
        if matches!(
            arg.as_str(),
            "-c" | "--count" | "--count-matches"
                | "-l" | "--files-with-matches"
                | "-n" | "--line-number" | "-N" | "--no-line-number"
                | "--heading" | "--no-heading"
                | "--json"
        ) {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Run the real ripgrep and capture its matches. ripgrep exits 1 on "no
/// matches", which is a normal empty result, not a failure — only a spawn
/// failure or an exit code above 1 is treated as `Failed`.
fn run_rg_capture(original: &[String]) -> RgCapture {
    let rg = match real_rg_path() {
        Some(p) => p,
        None => return RgCapture::Failed,
    };
    let output = match Command::new(&rg)
        .args(rg_capture_args(original))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return RgCapture::Failed,
    };
    if output.status.code().unwrap_or(2) > 1 {
        return RgCapture::Failed;
    }
    let matches = parse_rg_json(&String::from_utf8_lossy(&output.stdout));
    if matches.len() > MATCH_CAP {
        RgCapture::OverCap(matches)
    } else {
        RgCapture::Matches(matches)
    }
}
```

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS, 77 tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: capture real rg output as JSON for structural filtering"
```

---

### Task 7: Wire the inverted pipeline

The integration task. Replaces the redirect tail of `main()`, retargets `render_output` at `RgMatch`, and deletes the now-unreachable `run_ast_grep` and `run_rg_count`.

**Files:**
- Modify: `src/main.rs:404` (`main`), `src/main.rs:1098` (`render_output`), `src/main.rs:1136` (`run_ast_grep` → replaced), `src/main.rs:885` (`run_rg_count` → deleted)
- Test: `src/main.rs` `mod tests`

**Interfaces:**
- Consumes: everything from Tasks 1–6.
- Produces: `fn run_ast_grep_on_files(sg_pattern: &str, lang: &str, files: &[String]) -> Vec<AgMatch>`, `fn filter_matches(hits: &[RgMatch], ag: &[AgMatch]) -> (Vec<RgMatch>, usize)`, and `render_output(matches: &[RgMatch], mode: OutputMode) -> String` (parameter type changed from `&[AgMatch]`).

- [ ] **Step 1: Write the failing tests**

The five existing `render_output` tests construct `AgMatch` via `parse_ag_matches`; they must now build `RgMatch`. Replace those five with the versions below and add the `filter_matches` tests:

```rust
    fn rg_hit(file: &str, line: u64, text: &str) -> RgMatch {
        RgMatch { file: file.into(), line, text: text.into() }
    }

    #[test]
    fn content_output_is_file_colon_line_colon_text() {
        let m = vec![rg_hit("src/a.ts", 1, "const deviceId = 1;")];
        assert_eq!(
            render_output(&m, OutputMode::Content { line_numbers: true }),
            "src/a.ts:1:const deviceId = 1;\n"
        );
    }

    #[test]
    fn content_output_omits_the_line_number_when_not_asked_for() {
        let m = vec![rg_hit("src/a.ts", 1, "const deviceId = 1;")];
        assert_eq!(
            render_output(&m, OutputMode::Content { line_numbers: false }),
            "src/a.ts:const deviceId = 1;\n"
        );
    }

    #[test]
    fn count_output_is_per_file_like_ripgrep() {
        let m = vec![rg_hit("src/a.ts", 1, "a"), rg_hit("src/a.ts", 5, "b"), rg_hit("src/b.ts", 3, "c")];
        assert_eq!(
            render_output(&m, OutputMode::Count { show_filename: true }),
            "src/a.ts:2\nsrc/b.ts:1\n"
        );
    }

    #[test]
    fn count_output_omits_the_filename_for_a_single_explicit_file() {
        let m = vec![rg_hit("src/a.ts", 1, "a"), rg_hit("src/a.ts", 5, "b")];
        assert_eq!(render_output(&m, OutputMode::Count { show_filename: false }), "2\n");
    }

    #[test]
    fn files_with_matches_output_is_sorted_and_deduped() {
        let m = vec![rg_hit("src/b.ts", 1, "a"), rg_hit("src/a.ts", 1, "b"), rg_hit("src/b.ts", 4, "c")];
        assert_eq!(render_output(&m, OutputMode::FilesWithMatches), "src/a.ts\nsrc/b.ts\n");
    }

    // ── filtering ──

    #[test]
    fn filter_keeps_confirmed_hits_and_counts_the_rest() {
        let hits = vec![
            rg_hit("a.py", 1, "bypass_rls(conn)"),
            rg_hit("a.py", 7, "# bypass_rls is used above"),
            rg_hit("a.py", 9, "SQL = 'select bypass_rls'"),
        ];
        let ag = parse_ag_matches(&ag_json_span("a.py", 0, 0, "bypass_rls(conn)"));
        let (kept, suppressed) = filter_matches(&hits, &ag);
        assert_eq!(kept, vec![rg_hit("a.py", 1, "bypass_rls(conn)")]);
        assert_eq!(suppressed, 2, "the comment and the SQL string");
    }

    #[test]
    fn filter_with_no_confirmations_keeps_nothing() {
        let hits = vec![rg_hit("a.sql", 1, "-- tenant_id")];
        let (kept, suppressed) = filter_matches(&hits, &[]);
        assert!(kept.is_empty());
        assert_eq!(suppressed, 1);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test 2>&1 | tail -15`
Expected: compile errors — `cannot find function 'filter_matches'`, and `render_output` type mismatches (`expected &[AgMatch], found &[RgMatch]`).

- [ ] **Step 3: Change `render_output`, add the two new functions**

Change the signature at `src/main.rs:1098` — the body is unchanged, since `RgMatch` has the same `file`/`line`/`text` fields:

```rust
fn render_output(matches: &[RgMatch], mode: OutputMode) -> String {
```

Add after `confirmed_spans`/`is_confirmed`:

```rust
/// Split ripgrep's hits into the ones ast-grep structurally confirmed and a
/// count of the ones it did not (comments, strings, SQL, config).
fn filter_matches(hits: &[RgMatch], ag: &[AgMatch]) -> (Vec<RgMatch>, usize) {
    let spans = confirmed_spans(ag);
    let kept: Vec<RgMatch> = hits.iter().filter(|h| is_confirmed(h, &spans)).cloned().collect();
    let suppressed = hits.len() - kept.len();
    (kept, suppressed)
}

/// Run ast-grep over an EXPLICIT file list rather than a directory.
///
/// Two wins over walking a directory: ast-grep and ripgrep can no longer
/// disagree about ignore rules, and the language is known to be right because
/// it came from these files' own extensions. Errors yield no matches; the
/// caller falls back to showing ripgrep's hits, so a failure is never silent.
fn run_ast_grep_on_files(sg_pattern: &str, lang: &str, files: &[String]) -> Vec<AgMatch> {
    let mut cmd = Command::new("ast-grep");
    cmd.arg("run").arg("-p").arg(sg_pattern).arg("-l").arg(lang);
    for f in files {
        cmd.arg(f);
    }
    cmd.arg("--json=stream");
    match cmd.output() {
        Ok(o) => parse_ag_matches(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}
```

- [ ] **Step 4: Replace the redirect tail of `main()`**

In `main()` (`src/main.rs:404`), after the `inv` is parsed, replace every `exec_real_rg(&args[1..])` with `exec_real_rg(&rg_args)` where `rg_args` is defined immediately after parsing:

```rust
    let inv = parse_rg_invocation(&args[1..]);
    // `--no-smart` is ours; ripgrep would reject it.
    let rg_args = strip_shim_flags(&args[1..]);
```

Add the `--no-smart` guard beside the existing unsupported-flag guard:

```rust
    if inv.no_smart {
        log_event("passthrough", &pattern, "no_smart", None, 0);
        exec_real_rg(&rg_args);
    }
```

Then replace everything from `let lang_from_type = map_lang(...)` to the end of `main()` with:

```rust
    if !classify(&pattern) {
        log_event("passthrough", &pattern, "not_structural", None, 0);
        exec_real_rg(&rg_args);
    }

    // ① ripgrep first: ground truth, and it can never be silently empty.
    let rg_start = Instant::now();
    let hits = match run_rg_capture(&rg_args) {
        RgCapture::Failed => {
            log_event("passthrough", &pattern, "rg_failed", None, 0);
            exec_real_rg(&rg_args);
        }
        RgCapture::OverCap(hits) => {
            log_event("fallback", &pattern, "over_cap", None, hits.len() as u64);
            print!("{}", render_output(&hits, output_mode(&inv)));
            std::process::exit(0);
        }
        RgCapture::Matches(hits) => hits,
    };
    let rg_time_ms = rg_start.elapsed().as_millis() as u64;

    // ② Nothing to filter — ast-grep never spawns.
    if hits.is_empty() {
        log_event("no_match", &pattern, "rg_empty", None, 0);
        std::process::exit(1);
    }

    // ③ ast-grep filters, once per language actually present in the hits.
    let mut files: Vec<String> = hits.iter().map(|h| h.file.clone()).collect();
    files.sort();
    files.dedup();
    let by_lang = group_files_by_lang(&files);
    let sg_pattern = translate_pattern(&pattern);

    let ag_start = Instant::now();
    let mut ag: Vec<AgMatch> = Vec::new();
    for (lang, lang_files) in &by_lang {
        ag.extend(run_ast_grep_on_files(&sg_pattern, lang, lang_files));
    }
    let ag_time_ms = ag_start.elapsed().as_millis() as u64;
    let langs: Vec<&str> = by_lang.keys().copied().collect();
    let lang_label = langs.join(",");

    let (kept, suppressed) = filter_matches(&hits, &ag);

    // ④ ast-grep confirmed nothing — show every ripgrep hit rather than an
    // empty answer, and do NOT credit a win.
    if kept.is_empty() {
        log_event("fallback", &pattern, "ast_grep_empty", Some(&lang_label), hits.len() as u64);
        print!("{}", render_output(&hits, output_mode(&inv)));
        std::process::exit(0);
    }

    if suppressed > 0 {
        eprintln!(
            "\x1b[36msmart-rg: {suppressed} text-only match{} suppressed \
             (comments/strings/SQL) — rerun with --no-smart\x1b[0m",
            if suppressed == 1 { "" } else { "es" }
        );
    }
    print!("{}", render_output(&kept, output_mode(&inv)));

    let kept_files: HashSet<&str> = kept.iter().map(|k| k.file.as_str()).collect();
    log_event("structural", &sg_pattern, "filtered", Some(&lang_label), kept.len() as u64);
    log_comparison(
        &pattern, &lang_label,
        kept.len() as u64, kept_files.len() as u64, ag_time_ms,
        hits.len() as u64, files.len() as u64, rg_time_ms,
    );
}
```

Add the `output_mode` helper next to `render_output` (it was inline in the old `run_ast_grep`):

```rust
/// The output shape the caller asked for.
fn output_mode(inv: &RgInvocation) -> OutputMode {
    if inv.count {
        // rg omits the path prefix only when exactly one FILE was named.
        let single_file = inv.paths.len() == 1 && std::path::Path::new(&inv.paths[0]).is_file();
        OutputMode::Count { show_filename: !single_file }
    } else if inv.files_with_matches {
        OutputMode::FilesWithMatches
    } else {
        OutputMode::Content {
            line_numbers: inv.line_numbers.unwrap_or_else(|| std::io::stdout().is_terminal()),
        }
    }
}
```

- [ ] **Step 5: Delete the now-unreachable code**

Delete `fn run_rg_count` (`src/main.rs:885`, ~110 lines) and `fn run_ast_grep` (`src/main.rs:1136`, ~90 lines). Nothing calls either after Step 4. Also delete `fn infer_lang_from_path`, `fn dominant_lang` and `fn walk_for_lang` **only if** `cargo build` reports them unused — `map_lang` is still used for `--type`, so check before removing. Keep `dominant_lang`'s five tests only if the function survives; otherwise delete them with it.

Run: `cargo build --release 2>&1 | grep -E "never used|warning" | head`
Expected: no `never used` warnings remain.

- [ ] **Step 6: Run the full suite**

Run: `cargo test 2>&1 | tail -5`
Expected: PASS. Count depends on whether the `dominant_lang` tests were removed in Step 5.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "feat: invert the pipeline — rg first, ast-grep filters

rg runs first as ground truth; ast-grep confirms which hits are structural,
running once per language actually present in the matched files. A search rg
cannot find never reaches ast-grep. Suppressed text-only matches are reported
on stderr, never dropped silently.

Deletes run_rg_count and run_ast_grep, both unreachable: the comparison
baseline is now a byproduct of the rg run rather than a third spawn."
```

---

### Task 8: End-to-end verification, changelog, release

**Files:**
- Create: `/tmp/rgfirst/` fixtures (scratch, not committed)
- Modify: `CHANGELOG.md`, `Cargo.toml`, `Cargo.lock`

**Interfaces:**
- Consumes: the release binary from Task 7.

- [ ] **Step 1: Build and create the fixtures**

```bash
cargo build --release
SP=/tmp/rgfirst && rm -rf $SP && mkdir -p $SP/poly $SP/home/bin
ln -sf "$(command -v rg | grep -v smart-rg || echo /opt/homebrew/bin/rg)" $SP/home/bin/rg2
cat > $SP/poly/svc.py <<'EOF'
def handler():
    return arming_snapshot(session)
# arming_snapshot is described in this comment
EOF
cat > $SP/poly/web.ts <<'EOF'
export function read() { return arming_snapshot(ctx); }
EOF
cat > $SP/poly/ml.py <<'EOF'
result = bypass_rls(
    session, tenant_id
)
SQL = "select bypass_rls from t"
EOF
```

- [ ] **Step 2: Verify the polyglot case (the headline fix)**

```bash
cd /tmp/rgfirst && SMART_RG_HOME=/tmp/rgfirst/home \
  /Users/user/Documents/Projects/sandbox/smart-rg-shim/target/release/smart-rg \
  -n 'arming_snapshot\(' poly
```
Expected: hits in **both** `poly/svc.py` and `poly/web.ts`, and the comment on `svc.py:3` suppressed with a stderr note. Before this change, one dominant language was guessed and the other file was invisible.

- [ ] **Step 3: Verify containment on the multi-line call**

```bash
cd /tmp/rgfirst && SMART_RG_HOME=/tmp/rgfirst/home \
  /Users/user/Documents/Projects/sandbox/smart-rg-shim/target/release/smart-rg \
  -n tenant_id poly/ml.py
```
Expected: `poly/ml.py:2:    session, tenant_id` is **kept** — it sits on a continuation line of the `bypass_rls(...)` node, not its start line. If this line is missing, `is_confirmed` regressed to start-line equality.

- [ ] **Step 4: Verify the v0.3.13 flag matrix is untouched**

```bash
cd /tmp/rgfirst && export SMART_RG_HOME=/tmp/rgfirst/home
SHIM=/Users/user/Documents/Projects/sandbox/smart-rg-shim/target/release/smart-rg
RG=$(readlink $SMART_RG_HOME/bin/rg2)
for spec in "-v arming_snapshot poly" "-A2 arming_snapshot poly" "-i ARMING_SNAPSHOT poly" \
            "-m1 arming_snapshot poly" "-o arming_snapshot poly" "-F arming_snapshot poly" \
            "-C 1 arming_snapshot poly" "--invert-match arming_snapshot poly" \
            "--files-without-match arming_snapshot poly"; do
  a=$(eval "$RG $spec" 2>/dev/null); b=$(eval "$SHIM $spec" 2>/dev/null)
  [ "$a" = "$b" ] && echo "✅ rg $spec" || echo "❌ rg $spec"
done
```
Expected: nine ✅. These flags passthrough before any of the new code runs.

- [ ] **Step 5: Verify `--no-smart` and the telemetry**

```bash
cd /tmp/rgfirst && SMART_RG_HOME=/tmp/rgfirst/home \
  /Users/user/Documents/Projects/sandbox/smart-rg-shim/target/release/smart-rg \
  --no-smart -n 'arming_snapshot\(' poly
sqlite3 -header -column /tmp/rgfirst/home/stats.db \
  "SELECT event, reason, lang, matches FROM events ORDER BY id;"
```
Expected: `--no-smart` prints ripgrep's full output including the comment line, and no rg usage error. The events table shows `structural/filtered`, `passthrough/no_smart`, and — for a pattern with no hits — `no_match/rg_empty`.

- [ ] **Step 6: Bump the version and write the changelog**

Set `version = "0.3.15"` in `Cargo.toml`, run `cargo build --release` to sync `Cargo.lock`, and add a `## [0.3.15]` entry to `CHANGELOG.md` above `## [0.3.14]` covering: the inverted pipeline, language from matched files, the escaped-literal `classify` fix, the stderr suppression notice, `--no-smart`, the new `no_match` event type, and the deletion of `run_rg_count`/`run_ast_grep`.

- [ ] **Step 7: Commit and open the PR**

```bash
cargo test
git add -A
git commit -m "chore: v0.3.15 — rg-first filter pipeline"
git push -u origin feat/rg-first-filter
gh pr create --repo PKM-M84/shim --base main --title "feat: rg-first, ast-grep-as-filter pipeline (v0.3.15)" --body "Implements docs/specs/2026-07-26-rg-first-filter-design.md. Closes the redirect-effectiveness gap from #14."
```

Do **not** pass `--delete-branch` when merging if any branch is stacked on this one.

---

## Self-Review

**Spec coverage.** Inverted flow → Tasks 6, 7. Language from matched files → Task 5. Containment → Task 3. Filter policy + stderr note → Task 7. Output rendering via `OutputMode` → Task 7. Exit codes → Task 7 (1 on no hits, 0 otherwise). `--no-smart` → Tasks 4, 7. `classify` escaped-literal rule → Task 1. Telemetry (`structural`/`fallback`/`no_match`) → Task 7. Memory cap without re-exec → Task 6 (`OverCap` retains the matches). Testing → Tasks 1–7 unit, Task 8 end-to-end including the nine-flag regression. Out-of-scope alternations → untouched, they fail `literal_form`.

**Placeholders.** None: every step carries runnable code or a runnable command.

**Type consistency.** `RgMatch{file,line,text}` (Task 2) is consumed unchanged by `is_confirmed` (3), `filter_matches` (7) and `render_output` (7). `AgMatch` gains `end_line` in Task 3 and is only ever a filter input thereafter. `render_output`'s parameter changes from `&[AgMatch]` to `&[RgMatch]` in Task 7, and Task 7 Step 1 rewrites the five existing tests that construct the old type — the one signature change that ripples backwards, called out explicitly.

**Known follow-up, not in this plan.** `src/main.rs` is ~2,265 lines and this work adds more. A split into modules (`parse`, `search`, `stats`, `report`) is worth doing, but bundling it with a behaviour change this large would make review much harder.
