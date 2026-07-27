// smart-rg: A drop-in rg replacement that redirects structural code searches
// to ast-grep. Claude Code / Hermes / any coding agent compatible.
//
// Architecture: ripgrep runs FIRST, as ground truth — it can never be silently
// empty. ast-grep then FILTERS those hits down to the structural ones, once
// per language actually present in the matched files.
//
//   Input (rg flags) → Classify pattern → Structural? → rg (captured, ground truth)
//                                                          → ast-grep filters, per language
//                                                          → Output (confirmed ∪ unsearched)
//                                       → Text?          → real rg → Output
//
// A hit is kept when ast-grep confirmed it, OR when its file/language was
// never actually searched (unmapped extension, or ast-grep found nothing at
// all for that language — indistinguishable from a spawn/grammar failure).
//
// Stats:  smart-rg stats [--json]
//         smart-rg report [-o path.html]

use clap::{Parser, Subcommand};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ── Home directory ───────────────────────────────────────────

fn shim_home() -> PathBuf {
    std::env::var("SMART_RG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".smart-rg")
        })
}

fn db_path() -> PathBuf {
    shim_home().join("stats.db")
}

// ── Real ripgrep resolution ──────────────────────────────────
//
// Loop-prevention: the installer prepends ~/.smart-rg/bin (which holds THIS
// shim, named `rg`) to PATH position 1. So a bare "rg" PATH lookup would resolve
// straight back to us and re-exec forever — a fork bomb on Linux. We therefore
// (1) prefer the installer-written symlink ~/.smart-rg/bin/rg2 (points at the
// genuine ripgrep), and (2) otherwise scan PATH for an `rg` whose canonical path
// is neither this executable nor inside ~/.smart-rg/bin. We never fall back to a
// bare "rg".
fn real_rg_path() -> Option<PathBuf> {
    let shim_bin = shim_home().join("bin");

    // 1. Prefer the installer-written real-rg symlink.
    let rg2 = shim_bin.join("rg2");
    if is_executable_file(&rg2) {
        return Some(rg2);
    }

    // 2. Scan PATH for the first `rg` that is provably not the shim.
    let self_exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let shim_bin_canon = shim_bin.canonicalize().ok();
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("rg");
            if !is_executable_file(&cand) {
                continue;
            }
            let canon = match cand.canonicalize() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if self_exe.as_ref() == Some(&canon) {
                continue; // this is the shim itself
            }
            if let Some(ref sb) = shim_bin_canon {
                if canon.starts_with(sb) {
                    continue; // lives in ~/.smart-rg/bin
                }
            }
            return Some(canon);
        }
    }

    None
}

fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

fn ensure_home() {
    let _ = std::fs::create_dir_all(shim_home());
}

// ── CLI ──────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "smart-rg", version = env!("CARGO_PKG_VERSION"))]
#[command(disable_help_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show interception statistics (terminal)
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Generate a self-contained HTML report
    Report {
        #[arg(short = 'o', long = "output", default_value = "shim-stats.html")]
        output: String,
        /// Open in browser after generating
        #[arg(long)]
        open: bool,
    },
    /// Delete logged events older than N days (comparisons are kept)
    Prune {
        #[arg(long, default_value_t = 30)]
        days: u64,
    },
    /// Wipe ALL stats — events AND comparisons (incl. any seeded benchmark). Requires --yes.
    Reset {
        #[arg(long)]
        yes: bool,
    },
}

// ── Flag-agnostic rg argument extraction ─────────────────────
//
// We deliberately do NOT enumerate ripgrep's ~150 flags. That was an unwinnable
// game: every release added flags Claude Code happened to use, and any one we
// missed made clap abort the whole parse → the pattern was never seen → the call
// fell to a lossy `clap_unparsed` fallback (≈67% of all calls). Instead we parse
// only what the shim actually needs — pattern, search path, --type, and the two
// output-mode booleans (-c, -l) — and treat every other token as an opaque,
// harmless flag. The one small enumeration that remains is "which flags take a
// VALUE" (so we don't mistake a flag's value for the pattern); it is ~30 stable
// entries, and an omission is non-fatal (it can only mislabel the logged pattern,
// never change the user's actual search, which always forwards the ORIGINAL args).
#[derive(Debug, Default, PartialEq)]
struct RgInvocation {
    pattern: Option<String>,
    path: String,
    file_type: Option<String>,
    count: bool,
    files_with_matches: bool,
    // --files / --type-list: rg modes that take NO pattern. Positionals are all
    // paths; pattern stays None so main forwards the call verbatim (unlogged).
    pattern_less: bool,
    // Whether an explicit positional PATH was given (vs the default "."). Lets
    // main detect stream-filter calls (`cmd | rg PATTERN` with no path) that read
    // stdin and therefore cannot be redirected to ast-grep (file-only search).
    has_path: bool,
    // A positional `-` (ripgrep's explicit stdin marker). ast-grep cannot read
    // stdin, so any call that reads stdin must pass through to real rg — even
    // when stdin is a TTY (the user asked for it explicitly).
    reads_stdin: bool,
    // EVERY positional path (minus `-`). `ast-grep run` takes multiple [PATHS],
    // so all of them are forwarded; searching only the first silently dropped
    // whole directories from the answer.
    paths: Vec<String>,
    // `-g/--glob` values, forwarded to ast-grep's `--globs` (same gitignore-style
    // syntax including `!` negation), so a file filter the caller asked for is
    // actually applied instead of ignored.
    globs: Vec<String>,
    // The first flag found whose SEMANTICS ast-grep cannot reproduce. Set → the
    // whole call must go to real rg (see UNSUPPORTED_LONG / unsupported_short).
    unsupported: Option<String>,
    /// `--no-smart`: force plain ripgrep, no structural filtering.
    no_smart: bool,
    // Indices into the args slice of tokens that belong to the SHIM, not to
    // ripgrep, and must never be forwarded. Recorded by the parser so the
    // stripper cannot disagree with it about `--` or value-taking flags.
    shim_flag_indices: Vec<usize>,
    // -n/--line-number → Some(true), -N/--no-line-number → Some(false), unset →
    // None, meaning rg's own default (on for a TTY, off when piped).
    line_numbers: Option<bool>,
    // The argv for the internal `rg --json` capture run: the caller's tokens with
    // output-mode flags removed (or rewritten, for bundles that also carry a
    // filter) and shim-owned flags removed. Built during the SAME walk that
    // parses everything else, so it cannot disagree with that parse about the
    // `--` boundary or about which tokens are flag VALUES.
    capture_argv: Vec<String>,
}

// Flags that change WHICH lines match, or the shape of the output, in a way
// ast-grep has no equivalent for. A redirect would silently answer a different
// question than the caller asked — `-v` most starkly, which returned the
// matching lines instead of the non-matching ones. Passing these through costs
// only a missed optimisation; redirecting them costs a wrong answer.
//
// This is a deny-list of SEMANTICS, not an attempt to enumerate rg's flag
// surface (the mistake that made the old clap parser brittle). Unknown flags
// stay harmless, exactly as before.
const UNSUPPORTED_LONG: &[&str] = &[
    // inverted selection
    "invert-match", "files-without-match",
    // context lines around a match
    "after-context", "before-context", "context",
    // altered match extent / capping
    "only-matching", "max-count",
    // case-folding: ast-grep matching is case-sensitive
    "ignore-case", "smart-case", "iglob",
    // literal-text and rewrite semantics
    "fixed-strings", "replace",
    // output shapes the shim does not emit
    "quiet", "json",
    // vimgrep wants file:line:COLUMN:text (shim does not emit COLUMN)
    // passthru wants every line; under --json those are context records that parse_rg_json discards
    "vimgrep", "passthru",
];

// Short forms of the same set. Case matters: `-c` (count) is supported,
// `-C` (context) is not.
fn unsupported_short(c: char) -> bool {
    matches!(c, 'v' | 'A' | 'B' | 'C' | 'o' | 'm' | 'i' | 'S' | 'F' | 'r' | 'q')
}

// Long flags that take a separate VALUE token (the `--flag value` form). The
// `--flag=value` form is handled inline and never consumes the next token. This
// is the ONLY flag enumeration the shim keeps — small and stable. An omission is
// non-fatal: at worst the LOGGED pattern is slightly off; the user's search is
// unaffected because passthrough always forwards the original args verbatim.
const LONG_VALUE_FLAGS: &[&str] = &[
    "--regexp", "--type", "--type-not", "--type-add", "--type-clear",
    "--glob", "--iglob",
    "--max-count", "--max-depth", "--maxdepth", "--max-filesize", "--max-columns",
    "--after-context", "--before-context", "--context",
    "--sort", "--sortr", "--color", "--colors", "--encoding", "--threads",
    "--field-match-separator", "--field-context-separator",
    "--context-separator", "--path-separator", "--line-separator",
    "--ignore-file", "--file", "--replace", "--pre", "--pre-glob",
    "--engine", "--dfa-size-limit", "--regex-size-limit", "--hostname-bin",
];

// Short flags that consume a value: -e regexp, -t type, -T type-not, -g glob,
// -m max-count, -A/-B/-C context, -M max-columns, -j threads, -f file,
// -r replace, -E encoding, -d max-depth.
fn short_takes_value(c: char) -> bool {
    matches!(c, 'e' | 't' | 'T' | 'g' | 'm' | 'A' | 'B' | 'C' | 'M' | 'j' | 'f' | 'r' | 'E' | 'd')
}

/// Remove tokens that belong to the shim rather than ripgrep.
///
/// Driven by indices the PARSER recorded, never by matching strings. A blind
/// string filter disagrees with ripgrep's grammar in two ways that both
/// corrupt the user's search: it deletes a literal `--no-smart` appearing
/// after `--` (where ripgrep says everything is positional), and it deletes
/// the token when it is the VALUE of a preceding flag such as `-e`.
fn strip_shim_flags(args: &[String], shim_indices: &[usize]) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(i, _)| !shim_indices.contains(i))
        .map(|(_, a)| a.clone())
        .collect()
}

// Long flags that select an OUTPUT MODE — the long-form counterpart of
// `is_output_mode_short` below. These must never reach the internal `--json`
// capture run: ripgrep's mode precedence lets them beat `--json` regardless of
// position, so the capture would come back as plain text, fail to parse, and
// look exactly like a genuine no-match.
fn is_output_mode_long(name: &str) -> bool {
    matches!(
        name,
        "count" | "count-matches" | "files-with-matches"
            | "line-number" | "no-line-number"
            | "heading" | "no-heading"
            | "json"
            // ripgrep's own top-level modes: these print plain text and exit 0,
            // so surviving into the --json capture would yield a silent false
            // empty. Such calls pass through upstream anyway; excluding them
            // here means capture_argv does not depend on that gate.
            | "files" | "type-list"
    )
}

fn parse_rg_invocation(args: &[String]) -> RgInvocation {
    let mut inv = RgInvocation { path: ".".into(), ..Default::default() };
    let mut positionals: Vec<String> = Vec::new();
    let mut explicit_pattern: Option<String> = None;
    // Every -e/--regexp AND -f/--file occurrence, counted unconditionally —
    // including the ones `explicit_pattern.is_none()` below discards. ast-grep
    // takes exactly one pattern; more than one SOURCE means the call is a
    // union query (`-e A -e B` == `A|B`) that no single ast-grep pattern can
    // express, so it must go to real rg whole (see the check after the loop).
    let mut pattern_sources: u32 = 0;
    let mut i = 0;

    while i < args.len() {
        // Snapshotted once per iteration so its two consumers can never
        // disagree about which token this pass is about: the `--no-smart`
        // arm below records it into shim_flag_indices, and is_shim reads
        // that same push back to decide whether THIS token belongs in
        // capture_argv. No test can catch a regression here directly
        // (--no-smart is absent from LONG_VALUE_FLAGS, so the
        // value-consuming `i += 1` never fires on that path) — the
        // invariant is held by this comment, not by coverage.
        let token_start = i;
        let a = &args[i];

        // Everything after `--` is positional, verbatim.
        if a == "--" {
            positionals.extend(args[i + 1..].iter().cloned());
            inv.capture_argv.push("--".to_string());
            inv.capture_argv.extend(args[i + 1..].iter().cloned());
            break;
        }

        // Long flag: --name or --name=value
        if let Some(rest) = a.strip_prefix("--") {
            if rest.is_empty() { i += 1; continue; }
            let (name, inline_val) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let full = format!("--{name}");
            let mut value: Option<String> = inline_val;
            let mut consumed_next = false;
            if value.is_none() && LONG_VALUE_FLAGS.contains(&full.as_str()) && i + 1 < args.len() {
                value = Some(args[i + 1].clone());
                consumed_next = true;
                i += 1; // consume the value token
            }
            if UNSUPPORTED_LONG.contains(&name) && inv.unsupported.is_none() {
                inv.unsupported = Some(full.clone());
            }
            match name {
                "regexp" | "file" => {
                    pattern_sources += 1;
                    // --file's argument is a FILE OF PATTERNS, not a pattern — the
                    // parser stores the filename in explicit_pattern, and every
                    // downstream consumer (classify, translate_pattern, ast-grep
                    // itself) would filter against that filename as if the user
                    // had typed it. Unconditional: even the FIRST --file must not
                    // redirect.
                    if name == "file" && inv.unsupported.is_none() {
                        inv.unsupported = Some("-f".to_string());
                    }
                    if explicit_pattern.is_none() { explicit_pattern = value.clone(); }
                }
                "type" => { if value.is_some() { inv.file_type = value.clone(); } }
                "glob" => { if let Some(v) = &value { inv.globs.push(v.clone()); } }
                "count" => inv.count = true,
                "files-with-matches" => inv.files_with_matches = true,
                "files" | "type-list" => inv.pattern_less = true,
                "line-number" => inv.line_numbers = Some(true),
                "no-line-number" => inv.line_numbers = Some(false),
                "no-smart" => {
                    inv.no_smart = true;
                    inv.shim_flag_indices.push(token_start);
                }
                _ => {}
            }
            // Capture argv: keep the token unless it is an output-mode flag (it
            // would beat --json in ripgrep's own mode precedence) or a shim-owned
            // flag. Shim ownership is read back from the index the match arm just
            // recorded, so there is ONE encoding of that fact, not two.
            let is_shim = inv.shim_flag_indices.last() == Some(&token_start);
            if !is_output_mode_long(name) && !is_shim {
                inv.capture_argv.push(a.clone());
                if consumed_next {
                    inv.capture_argv.push(value.clone().unwrap());
                }
            }
            i += 1;
            continue;
        }

        // Short flag(s): -x, -xyz bundle, -A3 (attached value), -e <value>.
        // (`-` alone is ripgrep's stdin marker — falls through to positional.)
        if a.len() >= 2 && a.starts_with('-') {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut consumed_next = false;
            let mut idx = 0;
            while idx < chars.len() {
                let c = chars[idx];
                if unsupported_short(c) && inv.unsupported.is_none() {
                    inv.unsupported = Some(format!("-{c}"));
                }
                match c {
                    'c' => inv.count = true,
                    'l' => inv.files_with_matches = true,
                    'n' => inv.line_numbers = Some(true),
                    'N' => inv.line_numbers = Some(false),
                    _ => {}
                }
                if short_takes_value(c) {
                    // Value = remainder of this token if any, else the next token.
                    let remainder: String = chars[idx + 1..].iter().collect();
                    let value = if !remainder.is_empty() {
                        remainder
                    } else if i + 1 < args.len() {
                        consumed_next = true;
                        args[i + 1].clone()
                    } else {
                        String::new()
                    };
                    match c {
                        'e' | 'f' => {
                            pattern_sources += 1;
                            // Same rule as --file above: -f's value is a file OF
                            // patterns, not a pattern.
                            if c == 'f' && inv.unsupported.is_none() {
                                inv.unsupported = Some("-f".to_string());
                            }
                            if explicit_pattern.is_none() { explicit_pattern = Some(value); }
                        }
                        't' => inv.file_type = Some(value),
                        'g' => inv.globs.push(value),
                        _ => {}
                    }
                    break; // the rest of the bundle is this flag's value
                }
                idx += 1;
            }
            // Capture argv: rewrite drops only output-mode chars, never a
            // value-taking char, so whenever a value was consumed the flag
            // itself survives the rewrite — push the value verbatim, unexamined.
            if let Some(rewritten) = rewrite_short_token(a) {
                inv.capture_argv.push(rewritten);
                if consumed_next {
                    inv.capture_argv.push(args[i + 1].clone());
                }
            }
            if consumed_next { i += 1; }
            i += 1;
            continue;
        }

        // Positional (pattern or path).
        positionals.push(a.clone());
        inv.capture_argv.push(a.clone());
        i += 1;
    }

    // More than one pattern SOURCE (`-e A -e B`, or any mix with -f/--file):
    // rg answers the union of all of them, but ast-grep only ever gets the
    // ONE pattern kept above, so filtering the union against a single
    // pattern silently drops every real hit the other sources produced. A -f
    // occurrence already forced passthrough above (unconditionally, even
    // alone); this additionally catches `-e A -e B` (no -f involved).
    if pattern_sources > 1 && inv.unsupported.is_none() {
        inv.unsupported = Some("-e (multiple patterns)".to_string());
    }

    // ripgrep semantics: with -e/-f the pattern is explicit and ALL positionals
    // are paths; otherwise the FIRST positional is the pattern and the rest are
    // paths. The shim only searches one path (the first); passthrough forwards all.
    // Pattern-less modes (--files, --type-list) have no pattern at all — every
    // positional is a path, and pattern=None routes the call to verbatim passthrough.
    let paths: &[String] = if inv.pattern_less {
        &positionals
    } else if explicit_pattern.is_some() {
        inv.pattern = explicit_pattern;
        &positionals
    } else if !positionals.is_empty() {
        inv.pattern = Some(positionals[0].clone());
        &positionals[1..]
    } else {
        &[]
    };
    // A positional `-` is stdin, not a path. Record it so main forwards the
    // call, and keep every real path — ast-grep searches all of them. `path`
    // stays the first one, which is what language inference reads.
    inv.reads_stdin = paths.iter().any(|p| p == "-");
    inv.paths = paths.iter().filter(|p| p.as_str() != "-").cloned().collect();
    if let Some(p) = inv.paths.first() {
        inv.path = p.clone();
        inv.has_path = true;
    }
    inv
}

/// True when this call is FILTERING A STREAM (piped stdin) rather than searching
/// files. ast-grep only searches file paths — it has no stdin-search mode — so
/// `cmd | rg PATTERN` (no explicit path, stdin not a TTY) MUST go to real rg, or
/// the piped data is silently dropped while ast-grep searches the cwd instead.
/// An explicit path (`rg PATTERN src/`) always searches files, so it stays
/// eligible for redirect even when the agent's stdin is not a TTY.
fn is_stream_filter(has_path: bool, stdin_is_tty: bool) -> bool {
    !has_path && !stdin_is_tty
}

// ── Main ─────────────────────────────────────────────────────

// Human-facing help for the `smart-rg` management command. (Invoked as `rg`,
// --help is forwarded to real ripgrep instead — see main.) Version comes from
// Cargo.toml at compile time so it never drifts.
fn print_shim_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!("\
smart-rg {v} — a drop-in ripgrep shim that redirects structural code
searches to ast-grep and logs the files, tokens, and cost it saves.

USAGE:
  smart-rg <pattern> [rg flags] [path]    search (drop-in for `rg`)
  smart-rg <command> [options]            manage stats & reports

COMMANDS:
  stats [--json]              show interception stats in the terminal
  report [-o FILE] [--open]   write a self-contained HTML savings report
  prune [--days N]            delete logged events older than N days (default 30)
  reset --yes                 wipe ALL stats (events + comparisons)
  help                        show this help

SEARCH (used as `rg`):
  Accepts ripgrep's flags (-n, -l, -i, -c, --type, -C, -g, …) and prints the
  same file:line:content output. Structural patterns are routed to ast-grep;
  plain-text searches pass through to real ripgrep.
  For the full ripgrep flag reference:  rg --help

MANAGE THE INSTALL (install.sh):
  ./install.sh --check        dry-run: show what an install/update would do
  ./install.sh                install or update (idempotent, no sudo)
  ./install.sh --uninstall    remove smart-rg (keeps stats; add --purge to wipe)

EXAMPLES:
  smart-rg 'useState(' --type ts ./src
  smart-rg report -o report.html --open
  smart-rg stats

Stats live in ~/.smart-rg/stats.db. Built on ripgrep and ast-grep.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Route to subcommands
    if args.len() >= 2 {
        match args[1].as_str() {
            "stats" => {
                let cli = Cli::parse_from(args.iter());
                if let Some(Commands::Stats { json }) = cli.command {
                    if json { print_stats_json() } else { print_stats_table() }
                    return;
                }
            }
            "report" => {
                let cli = Cli::parse_from(args.iter());
                if let Some(Commands::Report { output, open }) = cli.command {
                    generate_report(&output, open);
                    return;
                }
            }
            "prune" => {
                let cli = Cli::parse_from(args.iter());
                if let Some(Commands::Prune { days }) = cli.command {
                    match open_db() {
                        Some(conn) => {
                            let n = prune_old_events(&conn, days);
                            println!("🧹 Pruned {} event(s) older than {} day(s) from {}",
                                     n, days, db_path().display());
                        }
                        None => eprintln!("No stats database found."),
                    }
                    return;
                }
            }
            "reset" => {
                let cli = Cli::parse_from(args.iter());
                if let Some(Commands::Reset { yes }) = cli.command {
                    match open_db() {
                        Some(conn) => {
                            let ev: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap_or(0);
                            let cp: i64 = conn.query_row("SELECT COUNT(*) FROM comparisons", [], |r| r.get(0)).unwrap_or(0);
                            if yes {
                                let _ = conn.execute_batch("DELETE FROM events; DELETE FROM comparisons;");
                                println!("🧼 Reset: cleared {} event(s) and {} comparison(s). Starting clean.", ev, cp);
                            } else {
                                println!("This deletes ALL stats: {} event(s) + {} comparison(s) (incl. any seeded benchmark)", ev, cp);
                                println!("from {}.", db_path().display());
                                println!("Re-run to confirm:  smart-rg reset --yes");
                            }
                        }
                        None => eprintln!("No stats database found."),
                    }
                    return;
                }
            }
            _ => {}
        }
    }

    // Help. Invoked as `smart-rg`, show OUR help (subcommands + drop-in usage).
    // Invoked as `rg`, forward --help to real ripgrep so anything probing the
    // `rg` contract still sees ripgrep's own help. A bare `smart-rg` shows help;
    // a bare `rg` still passes through.
    let invoked_as_smart_rg = std::env::args().next()
        .map(|a0| std::path::Path::new(&a0).file_name()
            .map(|f| f.to_string_lossy() == "smart-rg").unwrap_or(false))
        .unwrap_or(false);
    let wants_help = args.iter().any(|a| a == "--help" || a == "-h")
        || args.get(1).map(|a| a == "help").unwrap_or(false);
    if invoked_as_smart_rg && (wants_help || args.len() <= 1) {
        print_shim_help();
        return;
    }

    // `smart-rg --version` reports the SHIM's version (matching Cargo.toml). Invoked
    // as `rg`, the same flag falls through to real ripgrep so the rg contract holds.
    let wants_version = args.iter().any(|a| a == "--version" || a == "-V")
        || args.get(1).map(|a| a == "version").unwrap_or(false);
    if invoked_as_smart_rg && wants_version {
        println!("smart-rg {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // Passthrough modes: no args, --help, -h
    if args.len() <= 1 || args.contains(&"--help".to_string()) || args.contains(&"-h".to_string()) {
        exec_real_rg(&args[1..]);
    }

    // Flag-agnostic extraction of only what the shim needs (pattern, path, type,
    // output mode). Unrecognised flags are ignored, never a parse failure — so a
    // new ripgrep flag can no longer knock a call onto a lossy fallback path. This
    // replaces the clap-derive struct that had to enumerate rg's whole flag surface.
    let inv = parse_rg_invocation(&args[1..]);

    // `--no-smart` is ours; ripgrep would reject it. Every passthrough below
    // forwards `rg_args`, never the raw argv.
    let rg_args = strip_shim_flags(&args[1..], &inv.shim_flag_indices);

    let pattern = match inv.pattern.as_ref() {
        Some(p) => p.clone(),
        // No search term (e.g. `--files`, `--version`, `--type-list`): forward as-is.
        None => exec_real_rg(&rg_args),
    };

    // Stream-filter guard: `cmd | rg PATTERN` reads stdin, which ast-grep cannot
    // search. Forward verbatim so the pipe is filtered correctly instead of being
    // silently dropped while ast-grep searches the cwd.
    // Explicit `-` stdin OR an implicit pipe (no path + non-TTY stdin): ast-grep
    // has no stdin-search mode, so forward verbatim or the stream is dropped.
    if inv.reads_stdin || is_stream_filter(inv.has_path, std::io::stdin().is_terminal()) {
        let reason = if inv.reads_stdin { "stdin_dash" } else { "stream_stdin" };
        log_event("passthrough", &pattern, reason, None, 0);
        exec_real_rg(&rg_args);
    }

    // Flag-fidelity guard: a flag whose semantics ast-grep cannot reproduce
    // (invert, context, case-folding, capping, …) would make a redirect answer a
    // DIFFERENT question than the caller asked. Forward the call verbatim so the
    // answer is right; the lost redirect is only a lost optimisation.
    if let Some(flag) = inv.unsupported.clone() {
        log_event("passthrough", &pattern, &format!("unsupported_flag_{flag}"), None, 0);
        exec_real_rg(&rg_args);
    }

    if inv.no_smart {
        log_event("passthrough", &pattern, "no_smart", None, 0);
        exec_real_rg(&rg_args);
    }

    if !classify(&pattern) {
        log_event("passthrough", &pattern, "not_structural", None, 0);
        exec_real_rg(&rg_args);
    }

    // ① ripgrep first: ground truth, and it can never be silently empty.
    let rg_start = Instant::now();
    let hits = match run_rg_capture(&inv.capture_argv) {
        RgCapture::Failed => {
            log_event("passthrough", &pattern, "rg_failed", None, 0);
            exec_real_rg(&rg_args);
        }
        RgCapture::Unparseable => {
            log_event("passthrough", &pattern, "rg_json_unparseable", None, 0);
            exec_real_rg(&rg_args);
        }
        RgCapture::OverCap(hits) => {
            // rg already ran — render what we have. Re-running it would
            // reintroduce the double-execution this design removes.
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
    files.dedup(); // rg emits one record per matching LINE, not per file
    let by_lang = group_files_by_lang(&files);

    if by_lang.is_empty() {
        // No hit file has a language ast-grep can parse. Nothing to filter.
        log_event("fallback", &pattern, "no_searchable_language", None, hits.len() as u64);
        print!("{}", render_output(&hits, output_mode(&inv)));
        std::process::exit(0);
    }

    let sg_pattern = translate_pattern(&pattern);

    let ag_start = Instant::now();
    // LANGUAGE granularity: this catches a whole language confirming nothing
    // (wrong grammar / spawn failure — indistinguishable, see
    // confirm_by_language). A file ast-grep silently skips WITHIN an otherwise
    // confirming language is not caught here and stays suppressed.
    let (ag, searched, confirming_langs) =
        confirm_by_language(&by_lang, |lang, files| run_ast_grep_on_files(&sg_pattern, lang, files));
    let ag_time_ms = ag_start.elapsed().as_millis() as u64;
    // Only languages that actually confirmed something are credited. Labelling
    // a run "css,html,python" for a single Python confirmation would corrupt
    // the Top-languages KPI, which groups on this string verbatim.
    let lang_label = confirming_langs.join(",");

    let out = filter_matches(&hits, &ag, &searched);

    // ④ ast-grep confirmed nothing in the files it searched — show every
    // ripgrep hit rather than an empty answer, and do NOT credit a win.
    if out.confirmed_hits == 0 {
        log_event("fallback", &pattern, "ast_grep_empty", Some(&lang_label), hits.len() as u64);
        print!("{}", render_output(&hits, output_mode(&inv)));
        std::process::exit(0);
    }
    // lang_label is non-empty from here on: out.confirmed_hits > 0 requires at
    // least one language in confirming_langs, since that is the only way
    // filter_matches counts a confirmation.

    if out.suppressed > 0 {
        eprintln!(
            "\x1b[36msmart-rg: {} match{} not confirmed as structural by ast-grep \
             — rerun with --no-smart\x1b[0m",
            out.suppressed,
            if out.suppressed == 1 { "" } else { "es" }
        );
    }
    print!("{}", render_output(&out.kept, output_mode(&inv)));

    let searched_files: HashSet<&str> = hits
        .iter()
        .map(|h| norm_path(h.file.as_str()))
        .filter(|f| searched.contains(*f))
        .collect();
    // Files ast-grep CONFIRMED at least one hit in. Must be a subset of the
    // searched files, or the report claims ast-grep searched more files than
    // ripgrep did — the inverse of this tool's premise. Deriving it from
    // `searched` also revives `files_saved`, which was pinned at 0 while both
    // counts measured the same set.
    let confirmed_files: HashSet<&str> = out
        .kept
        .iter()
        .map(|h| norm_path(h.file.as_str()))
        .filter(|f| searched.contains(*f))
        .collect();
    log_event("structural", &sg_pattern, "filtered", Some(&lang_label), out.confirmed_hits as u64);
    // Comparison covers SEARCHED files only: counting hits ast-grep never saw
    // would manufacture fake "noise avoided".
    log_comparison(
        &pattern, &lang_label,
        out.confirmed_hits as u64, confirmed_files.len() as u64, ag_time_ms,
        out.searched_hits as u64, searched_files.len() as u64, rg_time_ms,
    );
}

// ── Real rg executor ─────────────────────────────────────────

fn exec_real_rg(args: &[String]) -> ! {
    // Forward to the real ripgrep, never our shim on PATH (see real_rg_path:
    // a bare PATH lookup could resolve back to us and fork-bomb on Linux).
    let real_rg = match real_rg_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "smart-rg: real ripgrep not found; reinstall smart-rg or create \
                 ~/.smart-rg/bin/rg2 symlinked to your ripgrep binary"
            );
            std::process::exit(127);
        }
    };

    let status = Command::new(&real_rg)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(2)),
        Err(e) => {
            eprintln!("smart-rg: failed to exec real rg at '{}' ({})", real_rg.display(), e);
            std::process::exit(2);
        }
    }
}

// ── SQLite logging ───────────────────────────────────────────

fn init_db(conn: &Connection) {
    conn.execute_batch(
        "PRAGMA busy_timeout=3000;
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent TEXT NOT NULL DEFAULT 'unknown',
            event TEXT NOT NULL,
            pattern TEXT NOT NULL,
            reason TEXT NOT NULL DEFAULT '',
            lang TEXT NOT NULL DEFAULT '',
            matches INTEGER NOT NULL DEFAULT 0,
            ts TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_event ON events(event);
        CREATE INDEX IF NOT EXISTS idx_events_agent ON events(agent);
        CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
        CREATE TABLE IF NOT EXISTS comparisons (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            pattern TEXT NOT NULL,
            lang TEXT NOT NULL DEFAULT '',
            ag_matches INTEGER NOT NULL DEFAULT 0,
            ag_files INTEGER NOT NULL DEFAULT 0,
            ag_time_ms INTEGER NOT NULL DEFAULT 0,
            rg_results INTEGER NOT NULL DEFAULT 0,
            rg_files INTEGER NOT NULL DEFAULT 0,
            rg_time_ms INTEGER NOT NULL DEFAULT 0,
            files_saved INTEGER NOT NULL DEFAULT 0,
            estimated_tokens_saved INTEGER NOT NULL DEFAULT 0,
            estimated_cost_saved_cents REAL NOT NULL DEFAULT 0,
            ts TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_comparisons_ts ON comparisons(ts);"
    ).ok();

    // Idempotent column migrations. Run each independently so a column that
    // already exists doesn't abort the migrations that follow it.
    // text_tokens/ast_tokens + *_cost_cents let a comparison row carry the
    // real token/cost figures (e.g. from the benchmark lab) instead of the
    // live matches×15 estimate the report falls back to.
    for stmt in [
        "ALTER TABLE comparisons ADD COLUMN estimated_tokens_saved INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE comparisons ADD COLUMN estimated_cost_saved_cents REAL NOT NULL DEFAULT 0",
        "ALTER TABLE comparisons ADD COLUMN text_tokens INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE comparisons ADD COLUMN ast_tokens INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE comparisons ADD COLUMN text_cost_cents REAL NOT NULL DEFAULT 0",
        "ALTER TABLE comparisons ADD COLUMN ast_cost_cents REAL NOT NULL DEFAULT 0",
    ] {
        let _ = conn.execute(stmt, []);
    }
}

fn log_event(event_type: &str, pattern: &str, reason: &str, lang: Option<&str>, match_count: u64) {
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        ensure_home();
        let conn = Connection::open(db_path())?;
        init_db(&conn);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let ts = format!("{}.{:03}Z", now.as_secs(), now.subsec_millis());
        let agent = std::env::var("SMART_RG_AGENT").unwrap_or_else(|_| "unknown".into());
        let lang_str = lang.unwrap_or("");

        conn.execute(
            "INSERT INTO events (agent, event, pattern, reason, lang, matches, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![agent, event_type, pattern, reason, lang_str, match_count, ts],
        )?;

        // Retention is NOT done here (it would hold a write lock on every search,
        // hurting concurrent agents). Old events are pruned lazily by stats/report
        // and explicitly via `smart-rg prune`.
        Ok(())
    })();

    let _ = result;
}

// Delete events older than `days` days. Returns rows removed. (Comparisons are
// kept — they hold the benchmark/savings data the report is built on.)
fn prune_old_events(conn: &Connection, days: u64) -> usize {
    let cutoff = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days.saturating_mul(86400));
    conn.execute(
        "DELETE FROM events WHERE CAST(substr(ts, 1, instr(ts, '.') - 1) AS INTEGER) < ?1",
        rusqlite::params![cutoff],
    )
    .unwrap_or(0)
}

// ── Language mapping ─────────────────────────────────────────

fn ext_to_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("jsx"),
        "py" => Some("python"),
        "rs" => Some("rust"),
        "go" => Some("go"),
        "rb" => Some("ruby"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some("c"),
        "css" => Some("css"),
        "html" | "htm" => Some("html"),
        "swift" => Some("swift"),
        "kt" => Some("kotlin"),
        "scala" => Some("scala"),
        "php" => Some("php"),
        "sh" | "bash" => Some("bash"),
        _ => None,
    }
}

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

// ── Classification ───────────────────────────────────────────

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

    if raw.is_empty() || raw.len() <= 1 {
        return false;
    }

    // Structural indicators
    let has_mixed_case = raw.chars().any(|c| c.is_uppercase()) && raw.chars().any(|c| c.is_lowercase());
    let has_snake = raw.contains('_');
    // A dot signals structure (`obj.method`, `app.current_tenant_id`) only when it
    // sits BETWEEN identifier characters. A leading or trailing dot is a dotfile or
    // extension literal (`\.env`, `\.gitignore`) — a text search. Escaping used to
    // make these safe by accident, before literal_form existed.
    let chars: Vec<char> = raw.chars().collect();
    let has_interior_dot = chars.iter().enumerate().any(|(i, &c)| {
        c == '.'
            && i > 0
            && i + 1 < chars.len()
            && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')
            && (chars[i + 1].is_alphanumeric() || chars[i + 1] == '_')
    });
    let has_structural = has_interior_dot || raw.contains("::")
        || raw.contains("->") || raw.contains('(') || raw.contains(')');
    let has_space = raw.contains(' ');

    // Space-separated patterns without structural operators are text searches
    if has_space && !has_structural {
        return false;
    }

    // Reject pure-lowercase generic keywords — too broad for structural search
    if !has_mixed_case && !has_snake && !has_structural {
        return false;
    }

    // Function-call: "foo(", "obj.method(", or full signature "fn foo($$$)"
    if raw.contains('(') && !raw.contains('|') && !raw.contains('[') {
        return raw.ends_with('(') || raw.contains(".(") || raw.contains(')');
    }

    // Accept identifier-like forms OR any pattern with explicit structural operators
    let is_id_like = raw.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ' ' || c == '.');
    if is_id_like || has_structural {
        return has_mixed_case || has_snake || has_structural;
    }

    false
}

// ── Pattern translation ──────────────────────────────────────

// A pattern like `fn main(` / `function foo(` / `func Bar(` is a FUNCTION
// DEFINITION, not a call. Translating it to a call form (`fn main($$$)`) matches
// NOTHING — a body-less item isn't a complete node — and adding a body
// (`fn main($$$) { $$$ }`) misses every function that has a return type, since
// `-> T` / `: T` sits between the `)` and the `{`. The robust, language-uniform
// form is the bare `keyword name` signature: ast-grep matches the whole function
// item from its prefix regardless of return type or body (verified across Rust,
// TS, Go). The keyword must be followed by a name, so bare `func(` is a CALL to
// something *named* `func` and stays call-form.
fn is_fn_definition(stripped: &str) -> bool {
    let toks: Vec<&str> = stripped.split_whitespace().collect();
    toks.iter().enumerate().any(|(i, t)| {
        matches!(*t, "fn" | "function" | "func") && i + 1 < toks.len()
    })
}

fn translate_pattern(pattern: &str) -> String {
    let raw: String = pattern.chars().filter(|&c| c != '\\').collect();
    let raw = raw.trim();

    if let Some(stripped) = raw.strip_suffix('(') {
        if is_fn_definition(stripped) {
            return stripped.to_string();
        }
        return format!("{}($$$)", stripped);
    }

    if raw.contains(' ')
        && raw.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ' ')
    {
        return format!("{} $$$($$$) {{ $$$ }}", raw);
    }

    if raw.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
        return raw.to_string();
    }

    pattern.to_string()
}

// ── log_comparison (inserts into comparisons with ROI fields + rate limit) ─
fn log_comparison(
    pattern: &str,
    lang: &str,
    ag_matches: u64,
    ag_files: u64,
    ag_time_ms: u64,
    rg_results: u64,
    rg_files: u64,
    rg_time_ms: u64,
) {
    let files_saved = rg_files.saturating_sub(ag_files);
    let ast_tokens = ag_matches.saturating_mul(15);
    let text_tokens = rg_results.saturating_mul(15);
    let estimated_tokens_saved = text_tokens.saturating_sub(ast_tokens);
    // $2 per million tokens => cents = tokens * 0.0002
    let text_cost_cents = text_tokens as f64 * 0.0002;
    let ast_cost_cents = ast_tokens as f64 * 0.0002;
    // Clamp at 0: the shim never "costs" money. When ast-grep finds more real
    // matches than a literal text search (e.g. degenerate test patterns), the
    // raw difference is negative — but a negative "saving" is meaningless and
    // rendered the report untrustworthy (red cells). 0 is the honest floor.
    let estimated_cost_saved_cents = (text_cost_cents - ast_cost_cents).max(0.0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ts = format!("{}.{:03}Z", now.as_secs(), now.subsec_millis());

    let _ = (|| -> Result<(), Box<dyn std::error::Error>> {
        ensure_home();
        let conn = Connection::open(db_path())?;
        init_db(&conn);
        conn.execute(
            "INSERT INTO comparisons (pattern, lang, ag_matches, ag_files, ag_time_ms, rg_results, rg_files, rg_time_ms, files_saved, estimated_tokens_saved, estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents, ast_cost_cents, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            rusqlite::params![
                pattern, lang,
                ag_matches as i64, ag_files as i64, ag_time_ms as i64,
                rg_results as i64, rg_files as i64, rg_time_ms as i64, files_saved as i64,
                estimated_tokens_saved as i64, estimated_cost_saved_cents,
                text_tokens as i64, ast_tokens as i64, text_cost_cents, ast_cost_cents, ts
            ],
        )?;
        Ok(())
    })();
}

// ── ast-grep result parsing & ripgrep-shaped output ──────────

/// One matching line as ripgrep reported it. Distinct from `AgMatch`: this is a
/// LINE ripgrep hit, whereas an `AgMatch` is a syntax NODE ast-grep confirmed.
#[derive(Debug, PartialEq, Clone)]
struct RgMatch {
    file: String,
    line: u64,
    text: String,
}

/// Parse ripgrep's `--json` event stream, returning the matches and a count of
/// lines that could NOT be parsed: either not valid JSON at all, or a `match`
/// record whose fields could not be extracted.
///
/// `--json` is used rather than `path:line:text` because a path may itself
/// contain a colon. ripgrep emits `lines.bytes` (base64) instead of
/// `lines.text` when a matching line is not valid UTF-8, and `path.bytes` for
/// non-UTF-8 filenames. Those records carry a real hit we cannot represent.
/// Since this capture IS the user's answer, dropping them silently loses
/// search results — so the count is returned and the caller refuses to filter
/// when it is non-zero.
fn parse_rg_json(stdout: &str) -> (Vec<RgMatch>, usize) {
    let mut matches = Vec::new();
    let mut unparseable = 0usize;
    for line in stdout.lines() {
        let v: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            // A line ripgrep emitted that we cannot even parse as JSON is
            // exactly the case this return value exists to catch: it may hold
            // a real match we cannot represent, so it must be counted, not
            // silently skipped.
            Err(_) => {
                unparseable += 1;
                continue;
            }
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let parsed = (|| -> Option<RgMatch> {
            let d = v.get("data")?;
            Some(RgMatch {
                file: d.get("path")?.get("text")?.as_str()?.to_string(),
                line: d.get("line_number")?.as_u64()?,
                // ripgrep includes the trailing line terminator (\n on Unix, \r\n
                // on Windows). Strip both so rendering controls line breaks.
                text: d.get("lines")?.get("text")?.as_str()?
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string(),
            })
        })();
        match parsed {
            Some(m) => matches.push(m),
            None => unparseable += 1,
        }
    }
    (matches, unparseable)
}

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
    /// At least one `match` record could not be parsed, so the capture is not
    /// ground truth and must not be used to filter.
    Unparseable,
}

/// Short flags that select an OUTPUT MODE. These must never reach the internal
/// `--json` run: ripgrep's mode precedence lets `-c`/`-l` beat `--json` no
/// matter where it sits in the argv, so the capture would come back as plain
/// text, parse to zero matches, and look exactly like a genuine no-match.
fn is_output_mode_short(c: char) -> bool {
    matches!(c, 'c' | 'l' | 'n' | 'N')
}

/// Rewrite a short-flag token for the internal `--json` capture run, dropping
/// only the OUTPUT-MODE characters and keeping everything else.
///
/// Returns None when nothing survives (the token was purely output modes).
///
/// A bundle can legitimately mix modes with filters: `-cg` is count + glob,
/// `-ntrust` is line-numbers + `--type rust`, `-ce` is count + the pattern flag.
/// Dropping the whole token loses the filter — or, when the value sits in the
/// NEXT argv token, strands it as a bare positional and silently changes
/// ripgrep's pattern/path assignment. Scanning STOPS at the first value-taking
/// char, because everything after it belongs to that flag's value and must be
/// copied verbatim (`-tcpp` must never become `-tpp`).
fn rewrite_short_token(token: &str) -> Option<String> {
    let chars: Vec<char> = token.chars().skip(1).collect();
    let mut kept = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if short_takes_value(c) {
            // This flag and the rest of the token (its value) are copied as-is.
            kept.extend(&chars[i..]);
            break;
        }
        if !is_output_mode_short(c) {
            kept.push(c);
        }
        i += 1;
    }
    if kept.is_empty() {
        None
    } else {
        Some(format!("-{kept}"))
    }
}

/// Prepend `--json` to the capture argv the parser produced.
///
/// The argv itself is built by `parse_rg_invocation` during its own walk, so it
/// agrees with that parse about the `--` boundary and about which tokens are
/// flag VALUES. Re-deriving those roles from string shape here produced four
/// separate silent-wrong-search bugs before this was moved.
fn capture_command_args(capture_argv: &[String]) -> Vec<String> {
    let mut out = vec!["--json".to_string()];
    out.extend(capture_argv.iter().cloned());
    out
}

/// Run the real ripgrep and capture its matches. ripgrep exits 1 on "no
/// matches", which is a normal empty result, not a failure — only a spawn
/// failure or an exit code above 1 is treated as `Failed`.
fn run_rg_capture(capture_argv: &[String]) -> RgCapture {
    // PRECONDITION: only valid once main() has established that this call has a
    // pattern and carries no unsupported flag. ripgrep's own top-level modes
    // (--files, --type-list, -h, --generate) print plain text and exit 0, which
    // would parse to a silent false empty. main()'s pattern-None and --help
    // early returns are what exclude them; they must stay ahead of this call.
    debug_assert!(!capture_argv.is_empty(), "capture argv must carry a pattern");
    let rg = match real_rg_path() {
        Some(p) => p,
        None => return RgCapture::Failed,
    };
    let output = match Command::new(&rg)
        .args(capture_command_args(capture_argv))
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
    let (matches, unparseable) = parse_rg_json(&String::from_utf8_lossy(&output.stdout));
    if unparseable > 0 {
        return RgCapture::Unparseable;
    }
    if matches.len() > MATCH_CAP {
        RgCapture::OverCap(matches)
    } else {
        RgCapture::Matches(matches)
    }
}

#[derive(Debug, PartialEq)]
struct AgMatch {
    file: String,
    line: u64,
    /// Last line of the node (1-based, inclusive). A node may span several
    /// lines; see `is_confirmed`.
    end_line: u64,
    text: String,
}

enum OutputMode {
    /// `line_numbers` mirrors ripgrep: `file:line:text` with -n, `file:text`
    /// without. Emitting the line field unconditionally broke the rg contract
    /// for every caller that did not ask for it.
    Content { line_numbers: bool },
    /// `show_filename` mirrors ripgrep: a `path:count` line per file, except for
    /// a single explicitly-named FILE, where rg prints a bare count.
    Count { show_filename: bool },
    FilesWithMatches,
}

/// Parse ast-grep's `--json=stream` output (one JSON object per line).
fn parse_ag_matches(stdout: &str) -> Vec<AgMatch> {
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
            Some(AgMatch {
                file: v.get("file").and_then(|f| f.as_str()).unwrap_or("").to_string(),
                // ast-grep's range.start.line is 0-INDEXED; ripgrep reports
                // 1-indexed lines. Printing it raw pointed every redirected hit
                // one line above the real match.
                line: v
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0)
                    + 1,
                end_line: v
                    .get("range")
                    .and_then(|r| r.get("end"))
                    .and_then(|e| e.get("line"))
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0)
                    + 1,
                text: v
                    .get("lines")
                    .and_then(|l| l.as_str())
                    .or_else(|| v.get("text").and_then(|t| t.as_str()))
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Normalise a path string for cross-tool comparison.
///
/// ripgrep echoes the caller's path verbatim (`./src/a.ts`) while ast-grep
/// strips a leading `./` (`src/a.ts`). Keying the span map on raw strings
/// therefore misses on EVERY file whenever the caller passes a `./`-prefixed
/// path — Claude Code's canonical call shape — silently disabling filtering.
fn norm_path(p: &str) -> &str {
    let mut s = p;
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s
}

/// Line spans (1-based, inclusive) ast-grep confirmed, keyed by file.
fn confirmed_spans(matches: &[AgMatch]) -> HashMap<&str, Vec<(u64, u64)>> {
    let mut spans: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
    for m in matches {
        spans.entry(norm_path(m.file.as_str())).or_default().push((m.line, m.end_line));
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
        .get(norm_path(hit.file.as_str()))
        .is_some_and(|v| v.iter().any(|&(start, end)| hit.line >= start && hit.line <= end))
}

/// What survived filtering, plus the counts the telemetry needs.
struct FilterOutcome {
    /// Hits to print.
    kept: Vec<RgMatch>,
    /// Searched-but-unconfirmed hits (comments, strings, SQL) that were hidden.
    suppressed: usize,
    /// Hits in files ast-grep actually searched — the honest comparison denominator.
    searched_hits: usize,
    /// Of those, how many ast-grep confirmed.
    confirmed_hits: usize,
}

/// Split ripgrep's hits into what to print and what was hidden.
///
/// A hit is kept when ast-grep confirmed it, OR when its file was never searched
/// at all. `group_files_by_lang` drops any extension it cannot map (`.sql`,
/// `.md`, `.json`, and case-mismatched `.TS`), so those files reach ast-grep
/// never and can produce no confirmations. Suppressing them would silently
/// delete real results — `rg tenant_id src/` would print the `db.py` hits and
/// swallow every `schema.sql` one.
///
/// `searched` holds NORMALISED paths (`norm_path`), because ripgrep reports
/// `./db.py` where ast-grep reports `db.py`.
fn filter_matches(hits: &[RgMatch], ag: &[AgMatch], searched: &HashSet<String>) -> FilterOutcome {
    let spans = confirmed_spans(ag);
    let mut kept = Vec::new();
    let mut suppressed = 0usize;
    let mut searched_hits = 0usize;
    let mut confirmed_hits = 0usize;
    for h in hits {
        let was_searched = searched.contains(norm_path(h.file.as_str()));
        if !was_searched {
            kept.push(h.clone());
            continue;
        }
        searched_hits += 1;
        if is_confirmed(h, &spans) {
            confirmed_hits += 1;
            kept.push(h.clone());
        } else {
            suppressed += 1;
        }
    }
    FilterOutcome { kept, suppressed, searched_hits, confirmed_hits }
}

/// Run ast-grep over an EXPLICIT file list rather than a directory.
///
/// Two wins over walking a directory: ast-grep and ripgrep can no longer
/// disagree about ignore rules, and the language is known to be right because it
/// came from these files' own extensions. Errors yield no matches; the caller
/// then shows ripgrep's hits, so a failure is never silent.
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

/// Run ast-grep once per language and report which files were genuinely
/// searched, i.e. belong to a language that confirmed at least one match.
///
/// A language that confirms nothing is treated as NOT SEARCHED, because a
/// wrong-grammar run exits 1 with no output and NO stderr — indistinguishable
/// from a genuine empty. Its files' hits are then kept rather than suppressed.
/// The runner is injected so this decision is testable without spawning
/// ast-grep; `main()` passes `run_ast_grep_on_files`.
fn confirm_by_language<F>(
    by_lang: &BTreeMap<&'static str, Vec<String>>,
    mut run: F,
) -> (Vec<AgMatch>, HashSet<String>, Vec<&'static str>)
where
    F: FnMut(&str, &[String]) -> Vec<AgMatch>,
{
    let mut ag = Vec::new();
    let mut searched = HashSet::new();
    let mut confirming = Vec::new();
    for (lang, files) in by_lang {
        let found = run(lang, files);
        if found.is_empty() {
            continue;
        }
        searched.extend(files.iter().map(|f| norm_path(f.as_str()).to_string()));
        confirming.push(*lang);
        ag.extend(found);
    }
    (ag, searched, confirming)
}

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

/// Render matches in ripgrep's output shape for the requested mode. Files are
/// emitted in sorted order: ast-grep's parallel walk is nondeterministic, and a
/// stable order is what makes the output diffable.
fn render_output(matches: &[RgMatch], mode: OutputMode) -> String {
    let mut out = String::new();
    match mode {
        OutputMode::Content { line_numbers } => {
            for m in matches {
                if line_numbers {
                    out.push_str(&format!("{}:{}:{}\n", m.file, m.line, m.text));
                } else {
                    out.push_str(&format!("{}:{}\n", m.file, m.text));
                }
            }
        }
        OutputMode::Count { show_filename } => {
            if show_filename {
                let mut per_file: BTreeMap<&str, u64> = BTreeMap::new();
                for m in matches {
                    *per_file.entry(m.file.as_str()).or_insert(0) += 1;
                }
                for (file, count) in per_file {
                    out.push_str(&format!("{file}:{count}\n"));
                }
            } else {
                out.push_str(&format!("{}\n", matches.len()));
            }
        }
        OutputMode::FilesWithMatches => {
            let files: std::collections::BTreeSet<&str> =
                matches.iter().map(|m| m.file.as_str()).collect();
            for f in files {
                out.push_str(&format!("{f}\n"));
            }
        }
    }
    out
}

// ══════════════════════════════════════════════════════════════
//  STATS (from SQLite)
// ══════════════════════════════════════════════════════════════

#[derive(serde::Serialize)]
struct StatsReport {
    total_intercepted: u64,
    structural: u64,
    passthrough: u64,
    errors: u64,
    /// rg found nothing at all — ast-grep never ran. Neither a win nor a
    /// failure; excluded from `redirect_rate`'s denominator (see
    /// `redirect_rate()`) so a batch of pure no-match searches can't drag the
    /// rate down for a change that had nothing to do with them.
    no_match: u64,
    redirect_rate: f64,
    total_matches_found: u64,
    // Primary headline metric: false-positive matches a naive text search would
    // have surfaced (comments/strings/partial hits) that ast-grep's structural
    // match correctly skipped — summed as max(0, rg_results − ag_matches).
    total_false_positives_avoided: u64,
    total_files_saved: u64,
    /// Real row count of the comparisons table. NOT `comparisons.len()`, which
    /// is only the page rendered in the detail table.
    comparison_runs: u64,
    /// Oldest record still held (YYYY-MM-DD) — the window the page describes.
    data_since: String,
    total_tokens_saved_estimate: u64,
    total_cost_saved_cents: f64,
    by_event: HashMap<String, u64>,
    by_agent: Vec<AgentStats>,
    by_language: HashMap<String, u64>,
    by_day: Vec<DayStats>,
    top_patterns: Vec<PatternStat>,
    recent_redirects: Vec<RecentEntry>,
    comparisons: Vec<ComparisonStat>,
}

#[derive(serde::Serialize)]
struct ComparisonStat {
    pattern: String,
    lang: String,
    ag_matches: u64,
    ag_files: u64,
    ag_time_ms: u64,
    rg_results: u64,
    rg_files: u64,
    rg_time_ms: u64,
    files_saved: u64,
    estimated_tokens_saved: u64,
    estimated_cost_saved_cents: f64,
    text_tokens: u64,
    ast_tokens: u64,
    text_cost_cents: f64,
    ast_cost_cents: f64,
}

#[derive(serde::Serialize)]
struct AgentStats {
    agent: String,
    total: u64,
    structural: u64,
    passthrough: u64,
}

#[derive(serde::Serialize)]
struct DayStats {
    day: String,
    total: u64,
    structural: u64,
    // Its own column, not folded into "total - structural" ("Text"): rg found
    // nothing, so there is no text/AST question to render for that search.
    no_match: u64,
}

#[derive(serde::Serialize)]
struct PatternStat {
    pattern: String,
    lang: String,
    count: u64,
}

#[derive(serde::Serialize)]
struct RecentEntry {
    pattern: String,
    lang: String,
    matches: u64,
    agent: String,
    ts: String,
}

fn open_db() -> Option<Connection> {
    ensure_home();
    let conn = Connection::open(db_path()).ok()?;
    init_db(&conn);
    Some(conn)
}

/// Whole-table savings totals.
///
/// Deliberately a SEPARATE query from the one that feeds the detail table. That
/// query is paginated (`ORDER BY id DESC LIMIT N`) and the totals used to be
/// summed inside its row loop, so every headline KPI silently described only
/// the newest page — "Comparison Runs" read exactly the page size for any
/// database past it, and a live 402-row DB reported 104 files saved of 4,748.
struct ComparisonTotals {
    runs: u64,
    files_saved: u64,
    false_positives: u64,
    tokens_saved: u64,
    cost_saved_cents: f64,
}

fn comparison_totals(conn: &Connection) -> ComparisonTotals {
    // Mirrors the report front-end's precedence (estimated_* when present, else
    // text − ast) and its never-negative floor — but over EVERY row, so the
    // headline totals and the detail table can no longer tell different stories.
    conn.query_row(
        "SELECT
            COUNT(*),
            COALESCE(SUM(files_saved), 0),
            COALESCE(SUM(MAX(rg_results - ag_matches, 0)), 0),
            COALESCE(SUM(CASE WHEN estimated_tokens_saved > 0
                              THEN estimated_tokens_saved
                              ELSE MAX(text_tokens - ast_tokens, 0) END), 0),
            COALESCE(SUM(CASE WHEN estimated_cost_saved_cents != 0.0
                              THEN MAX(estimated_cost_saved_cents, 0.0)
                              ELSE MAX(text_cost_cents - ast_cost_cents, 0.0) END), 0.0)
         FROM comparisons",
        [],
        |r| {
            Ok(ComparisonTotals {
                runs: r.get(0)?,
                files_saved: r.get(1)?,
                false_positives: r.get(2)?,
                tokens_saved: r.get(3)?,
                cost_saved_cents: r.get(4)?,
            })
        },
    )
    .unwrap_or(ComparisonTotals {
        runs: 0,
        files_saved: 0,
        false_positives: 0,
        tokens_saved: 0,
        cost_saved_cents: 0.0,
    })
}

/// How many comparison rows the detail table shows. The totals above cover the
/// whole table regardless; this only bounds the rendered rows.
const COMPARISON_PAGE: usize = 50;

/// Structural redirects as a fraction of searches that had something TO
/// redirect. `no_match` (rg found nothing; ast-grep never ran) is neither a
/// win nor a failure, so it must not sit in this denominator — leaving it in
/// distorts the rate toward zero for searches the change never touched.
fn redirect_rate(total: u64, structural: u64, no_match: u64) -> f64 {
    let denom = total.saturating_sub(no_match);
    if denom > 0 { structural as f64 / denom as f64 * 100.0 } else { 0.0 }
}

/// By-language counts for structural redirects. `lang` may be a comma-joined
/// list of every CONFIRMING language in one event (a genuinely polyglot
/// confirmation — see `lang_label` in main()), so each row is split on ','
/// and fanned out before accumulating: a "python,typescript" row credits
/// BOTH "python" and "typescript", instead of becoming its own bucket
/// distinct from either.
fn language_counts(conn: &Connection) -> HashMap<String, u64> {
    let mut counts = HashMap::new();
    let stmt = conn.prepare(
        "SELECT lang, COUNT(*) FROM events WHERE event='structural' AND lang != '' GROUP BY lang"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            let l: String = row.get(0)?;
            let c: u64 = row.get(1)?;
            Ok((l, c))
        }).ok();
        if let Some(rows) = rows {
            for (lang, cnt) in rows.flatten() {
                for single in lang.split(',').filter(|s| !s.is_empty()) {
                    *counts.entry(single.to_string()).or_insert(0) += cnt;
                }
            }
        }
    }
    counts
}

/// Top redirected patterns, same comma-split fan-out as `language_counts` —
/// otherwise a pattern that sometimes confirms alone and sometimes confirms
/// alongside another language fragments across two (pattern, lang) buckets
/// instead of one count per language it actually touched.
fn top_pattern_stats(conn: &Connection) -> Vec<PatternStat> {
    let mut merged: HashMap<(String, String), u64> = HashMap::new();
    let stmt = conn.prepare(
        "SELECT pattern, lang, COUNT(*) as cnt FROM events WHERE event='structural' GROUP BY pattern, lang"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            let p: String = row.get(0)?;
            let l: String = row.get(1)?;
            let c: u64 = row.get(2)?;
            Ok((p, l, c))
        }).ok();
        if let Some(rows) = rows {
            for (pattern, lang, cnt) in rows.flatten() {
                for single in lang.split(',').filter(|s| !s.is_empty()) {
                    *merged.entry((pattern.clone(), single.to_string())).or_insert(0) += cnt;
                }
            }
        }
    }
    let mut top: Vec<PatternStat> = merged
        .into_iter()
        .map(|((pattern, lang), count)| PatternStat { pattern, lang, count })
        .collect();
    top.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.pattern.cmp(&b.pattern)));
    top.truncate(10);
    top
}

/// Per-day totals, formatted date not epoch. `no_match` is its own column —
/// see DayStats — so the time chart can give it its own series instead of
/// folding it into "Text".
fn day_counts(conn: &Connection) -> Vec<DayStats> {
    let mut by_day = Vec::new();
    let stmt = conn.prepare(
        "SELECT date(substr(ts, 1, 10), 'unixepoch') as day,
                COUNT(*) as total,
                COUNT(CASE WHEN event='structural' THEN 1 END) as structural,
                COUNT(CASE WHEN event='no_match' THEN 1 END) as no_match
         FROM events GROUP BY day ORDER BY day"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            Ok(DayStats {
                day: row.get(0)?,
                total: row.get(1)?,
                structural: row.get(2)?,
                no_match: row.get(3)?,
            })
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { by_day.push(r); }
        }
    }
    by_day
}

fn compute_stats() -> StatsReport {
    let conn = match open_db() {
        Some(c) => c,
        None => return empty_stats(),
    };

    // NOTE: generating a report no longer prunes. The old lazy `prune_old_events`
    // here silently deleted events older than 30 days every time the report was
    // viewed, while `comparisons` was never pruned — so one page mixed a 30-day
    // event window with an all-time comparison table. Retention stays available
    // as the explicit `smart-rg prune` command.

    let total: u64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap_or(0);
    if total == 0 {
        return empty_stats();
    }

    // The single window the whole page describes: the oldest record we still hold.
    let data_since: String = conn
        .query_row(
            "SELECT date(MIN(CAST(substr(ts, 1, instr(ts, '.') - 1) AS INTEGER)), 'unixepoch')
             FROM (SELECT ts FROM events UNION ALL SELECT ts FROM comparisons)",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();

    let structural: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event='structural'", [], |r| r.get(0)
    ).unwrap_or(0);

    let passthrough: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event='passthrough'", [], |r| r.get(0)
    ).unwrap_or(0);

    let errors: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event LIKE '%error%' OR event='untranslatable' OR event='fallback'",
        [], |r| r.get(0)
    ).unwrap_or(0);

    let no_match: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event='no_match'", [], |r| r.get(0)
    ).unwrap_or(0);

    let rate = redirect_rate(total, structural, no_match);

    let total_matches: u64 = conn.query_row(
        "SELECT COALESCE(SUM(matches), 0) FROM events WHERE event='structural'",
        [], |r| r.get(0)
    ).unwrap_or(0);

    // By event type
    let mut by_event = HashMap::new();
    let stmt = conn.prepare("SELECT event, COUNT(*) FROM events GROUP BY event").ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            let e: String = row.get(0)?;
            let c: u64 = row.get(1)?;
            Ok((e, c))
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { by_event.insert(r.0, r.1); }
        }
    }

    // By agent
    let mut by_agent = Vec::new();
    let stmt = conn.prepare(
        "SELECT agent, COUNT(*) as total,
                COUNT(CASE WHEN event='structural' THEN 1 END) as structural,
                COUNT(CASE WHEN event='passthrough' THEN 1 END) as passthrough
         FROM events GROUP BY agent ORDER BY total DESC"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            Ok(AgentStats {
                agent: row.get(0)?,
                total: row.get(1)?,
                structural: row.get(2)?,
                passthrough: row.get(3)?,
            })
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { by_agent.push(r); }
        }
    }

    // By language (structural only) — see language_counts for why the raw
    // comma-joined `lang` column is split before aggregating.
    let by_language = language_counts(&conn);

    // By day (formatted date, not epoch)
    let by_day = day_counts(&conn);

    // Top patterns — see top_pattern_stats for the same comma-split fan-out.
    let top_patterns = top_pattern_stats(&conn);

    // Recent redirects (with formatted timestamp)
    let mut recent = Vec::new();
    let stmt = conn.prepare(
        "SELECT pattern, lang, matches, agent,
               datetime(CAST(substr(ts, 1, instr(ts, '.') - 1) AS INTEGER), 'unixepoch') as ts
         FROM events WHERE event='structural' ORDER BY id DESC LIMIT 15"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            Ok(RecentEntry {
                pattern: row.get(0)?,
                lang: row.get(1)?,
                matches: row.get(2)?,
                agent: row.get(3)?,
                ts: row.get(4)?,
            })
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { recent.push(r); }
        }
    }

    // Headline savings: computed over the WHOLE table, independent of the page
    // of rows the detail table renders below.
    let totals = comparison_totals(&conn);

    // Comparison rows for the detail table (most recent page).
    let mut comparisons = Vec::new();
    let stmt = conn.prepare(
        "SELECT pattern, lang, ag_matches, ag_files, ag_time_ms, rg_results, rg_files, rg_time_ms, files_saved, estimated_tokens_saved, estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents, ast_cost_cents
         FROM comparisons ORDER BY id DESC LIMIT ?1"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([COMPARISON_PAGE], |row| {
            let fs: u64 = row.get(8)?;
            let est_toks: u64 = row.get(9)?;
            let est_cost: f64 = row.get(10)?;
            let text_tokens: u64 = row.get(11)?;
            let ast_tokens: u64 = row.get(12)?;
            let text_cost: f64 = row.get(13)?;
            let ast_cost: f64 = row.get(14)?;
            let ag_matches: u64 = row.get(2)?;
            let rg_results: u64 = row.get(5)?;
            Ok(ComparisonStat {
                pattern: row.get(0)?,
                lang: row.get(1)?,
                ag_matches,
                ag_files: row.get(3)?,
                ag_time_ms: row.get(4)?,
                rg_results,
                rg_files: row.get(6)?,
                rg_time_ms: row.get(7)?,
                files_saved: fs,
                estimated_tokens_saved: est_toks,
                estimated_cost_saved_cents: est_cost,
                text_tokens,
                ast_tokens,
                text_cost_cents: text_cost,
                ast_cost_cents: ast_cost,
            })
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { comparisons.push(r); }
        }
    }

    StatsReport {
        total_intercepted: total,
        structural,
        passthrough,
        errors,
        no_match,
        redirect_rate: rate,
        total_matches_found: total_matches,
        total_false_positives_avoided: totals.false_positives,
        total_files_saved: totals.files_saved,
        total_tokens_saved_estimate: totals.tokens_saved,
        total_cost_saved_cents: totals.cost_saved_cents,
        comparison_runs: totals.runs,
        data_since,
        by_event,
        by_agent,
        by_language,
        by_day,
        top_patterns,
        recent_redirects: recent,
        comparisons,
    }
}

fn empty_stats() -> StatsReport {
    StatsReport {
        total_intercepted: 0, structural: 0, passthrough: 0, errors: 0, no_match: 0,
        redirect_rate: 0.0, total_matches_found: 0,
        total_false_positives_avoided: 0,
        total_files_saved: 0, total_tokens_saved_estimate: 0, total_cost_saved_cents: 0.0,
        comparison_runs: 0, data_since: String::new(),
        by_event: HashMap::new(), by_agent: vec![],
        by_language: HashMap::new(), by_day: vec![],
        top_patterns: vec![], recent_redirects: vec![],
        comparisons: vec![],
    }
}

// ── Terminal table output ────────────────────────────────────

fn print_stats_table() {
    let stats = compute_stats();

    println!();
    println!("\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1;36m  🪶  smart-rg  —  Shim Stats\x1b[0m");
    println!("\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!();

    println!("\x1b[1m  Overview\x1b[0m");
    println!("  ─────────────────────────────────────────");
    println!("  Total intercepted:    {:>6}", stats.total_intercepted);
    println!("  Structural redirects: {:>6}  ({:.1}% of searches with something to redirect)", stats.structural, stats.redirect_rate);
    println!("  Passed through (text):{:>6}", stats.passthrough);
    println!("  No match (rg empty):  {:>6}", stats.no_match);
    println!("  Errors/fallbacks:     {:>6}", stats.errors);
    println!("  Total matches found:  {:>6}", stats.total_matches_found);
    println!();

    if stats.total_intercepted == 0 {
        println!("\x1b[33m  No data yet. Start using smart-rg to see stats.\x1b[0m");
        println!();
        return;
    }

    if !stats.by_agent.is_empty() {
        println!("\x1b[1m  By Agent\x1b[0m");
        println!("  ─────────────────────────────────────────");
        println!("  {:<20} {:>6} {:>10} {:>6}", "AGENT", "TOTAL", "STRUCTURAL", "PASS");
        println!("  {:-<46}", "");
        for s in &stats.by_agent {
            println!("  {:<20} {:>6} {:>10} {:>6}", s.agent, s.total, s.structural, s.passthrough);
        }
        println!();
    }

    if !stats.by_language.is_empty() {
        println!("\x1b[1m  By Language (structural redirects)\x1b[0m");
        println!("  ─────────────────────────────────────────");
        let mut langs: Vec<_> = stats.by_language.iter().collect();
        langs.sort_by(|a, b| b.1.cmp(a.1));
        for (lang, count) in langs {
            println!("  {:<20} {:>6}", lang, count);
        }
        println!();
    }

    if !stats.by_day.is_empty() {
        println!("\x1b[1m  By Day\x1b[0m");
        println!("  ─────────────────────────────────────────");
        println!("  {:<12} {:>6} {:>10}", "DAY", "TOTAL", "REDIRECTS");
        println!("  {:-<32}", "");
        for ds in &stats.by_day {
            println!("  {:<12} {:>6} {:>10}", ds.day, ds.total, ds.structural);
        }
        println!();
    }

    if !stats.top_patterns.is_empty() {
        println!("\x1b[1m  Top Redirected Patterns\x1b[0m");
        println!("  ─────────────────────────────────────────");
        for ps in &stats.top_patterns {
            let lang_tag = if ps.lang.is_empty() { String::new() } else { format!(" [{}]", ps.lang) };
            println!("  {:<30} {:>3}x{}", ps.pattern, ps.count, lang_tag);
        }
        println!();
    }

    // Savings from rg vs ast-grep comparison
    if !stats.comparisons.is_empty() {
        println!("\x1b[1m  rg vs ag — File Savings\x1b[0m");
        println!("  ─────────────────────────────────────────");
        println!("  {:<25} {:>10} {:>10} {:>10} {:>10}", "PATTERN", "AG FILES", "RG FILES", "SAVED", "EST. TOKENS");
        println!("  {:-<70}", "");
        for c in &stats.comparisons {
            println!("  {:<25} {:>10} {:>10} {:>10} {:>10}",
                c.pattern, c.ag_files, c.rg_files, c.files_saved, c.estimated_tokens_saved);
        }
        println!();
        // Totals come from the whole table, never from the rows printed above —
        // summing the displayed page reported a fraction of the real figure and
        // labelled it "Total".
        if (stats.comparisons.len() as u64) < stats.comparison_runs {
            println!("  (showing the {} most recent of {} runs)",
                stats.comparisons.len(), stats.comparison_runs);
        }
        println!("  Total files saved:  {:>10}", stats.total_files_saved);
        println!("  Total tokens saved: {:>10}", stats.total_tokens_saved_estimate);
        println!();
    }
}

// ── JSON output ──────────────────────────────────────────────

fn print_stats_json() {
    let stats = compute_stats();
    println!("{}", serde_json::to_string_pretty(&stats).unwrap());
}

// ── HTML Report ──────────────────────────────────────────────

const REPORT_TEMPLATE: &str = include_str!("report.html");

fn generate_report(output_path: &str, open_browser: bool) {
    let stats = compute_stats();
    let mut data_json = serde_json::to_string(&stats).unwrap_or_else(|_| "{}".into());
    // Escape </ to prevent premature script tag closure (XSS prevention)
    data_json = data_json.replace("</", r"<\/");
    let html = REPORT_TEMPLATE
        .replace("__SHIM_DATA__", &data_json)
        // Stamp the report with the SAME version as the binary (Cargo.toml), so a
        // fresh build can never look un-deployed because the report shows an old
        // hardcoded version. This was a real source of "my changes didn't land".
        .replace("__SHIM_VERSION__", env!("CARGO_PKG_VERSION"));

    match std::fs::write(output_path, &html) {
        Ok(_) => {
            let abs = std::fs::canonicalize(output_path)
                .unwrap_or_else(|_| PathBuf::from(output_path));
            println!("\x1b[1;32m📊 Report saved: {}\x1b[0m", abs.display());
            println!("   Open this file in your browser to view the dashboard.");

            if open_browser {
                let _ = Command::new("open")
                    .arg(&abs)
                    .spawn();
                println!("   Opening in browser...");
            }
        }
        Err(e) => {
            eprintln!("\x1b[31mError writing report: {}\x1b[0m", e);
            std::process::exit(1);
        }
    }
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> RgInvocation {
        let owned: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();
        parse_rg_invocation(&owned)
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn explicit_e_flag_is_the_pattern_and_positionals_are_paths() {
        let inv = parse(&["-e", "useState(", "--type", "ts", "."]);
        assert_eq!(inv.pattern.as_deref(), Some("useState("));
        assert_eq!(inv.file_type.as_deref(), Some("ts"));
        assert_eq!(inv.path, ".");
    }

    #[test]
    fn claude_code_canonical_call_finds_pattern_past_value_flags() {
        // The real shape that used to fall to clap_unparsed.
        let inv = parse(&[
            "--no-ignore", "--sort", "path", "--no-heading",
            "--color", "never", "-g", "!.git", "useState(", "./src",
        ]);
        assert_eq!(inv.pattern.as_deref(), Some("useState("));
        assert_eq!(inv.path, "./src");
        assert_eq!(inv.file_type, None);
    }

    #[test]
    fn first_positional_is_pattern_rest_is_path() {
        let inv = parse(&["foo(", "./src"]);
        assert_eq!(inv.pattern.as_deref(), Some("foo("));
        assert_eq!(inv.path, "./src");
    }

    #[test]
    fn has_path_set_only_when_a_positional_path_is_given() {
        assert!(parse(&["foo(", "./src"]).has_path);
        assert!(!parse(&["foo("]).has_path); // no path → default "." → has_path=false
        assert!(!parse(&["-l", "pattern"]).has_path);
    }

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

    #[test]
    fn stream_filter_only_for_piped_stdin_without_a_path() {
        // `cmd | rg PATTERN` (no path, stdin not a tty) → filter the stream via real rg.
        assert!(is_stream_filter(false, false));
        // `rg PATTERN src/` (explicit path) → search files → eligible for redirect.
        assert!(!is_stream_filter(true, false));
        // `rg PATTERN` (interactive tty, no path) → rg searches cwd → redirect ok.
        assert!(!is_stream_filter(false, true));
        assert!(!is_stream_filter(true, true));
    }

    #[test]
    fn short_value_and_boolean_flags() {
        let inv = parse(&["-c", "-t", "rs", "Command::new(", "."]);
        assert!(inv.count);
        assert_eq!(inv.file_type.as_deref(), Some("rs"));
        assert_eq!(inv.pattern.as_deref(), Some("Command::new("));
        assert_eq!(inv.path, ".");
    }

    #[test]
    fn files_with_matches_short_flag_and_default_path() {
        let inv = parse(&["-l", "pattern"]);
        assert!(inv.files_with_matches);
        assert_eq!(inv.pattern.as_deref(), Some("pattern"));
        assert_eq!(inv.path, "."); // no path positional → default
    }

    #[test]
    fn inline_equals_value_does_not_consume_next_token() {
        let inv = parse(&["--type=ts", "useState(", "."]);
        assert_eq!(inv.file_type.as_deref(), Some("ts"));
        assert_eq!(inv.pattern.as_deref(), Some("useState("));
        assert_eq!(inv.path, ".");
    }

    #[test]
    fn bundled_boolean_short_flags_do_not_eat_the_pattern() {
        let inv = parse(&["-ni", "fn main("]);
        assert!(!inv.count);
        assert_eq!(inv.pattern.as_deref(), Some("fn main("));
    }

    #[test]
    fn unknown_flag_is_treated_as_boolean_not_an_abort() {
        // THE point of the rewrite: a flag we've never heard of must not swallow
        // the pattern or derail parsing.
        let inv = parse(&["--some-future-flag", "useState(", "."]);
        assert_eq!(inv.pattern.as_deref(), Some("useState("));
        assert_eq!(inv.path, ".");
    }

    #[test]
    fn double_dash_forces_remaining_as_positionals() {
        let inv = parse(&["-n", "--", "-weird-pattern", "src"]);
        assert_eq!(inv.pattern.as_deref(), Some("-weird-pattern"));
        assert_eq!(inv.path, "src");
    }

    #[test]
    fn only_flags_no_pattern() {
        let inv = parse(&["--version"]);
        assert_eq!(inv.pattern, None);
    }

    // ── pattern-less modes: --files / --type-list have NO pattern ──
    //
    // `rg --files <path>` lists files; the positional is a PATH, not a pattern.
    // Claude Code runs this shape constantly (plugin cache scans). Taking the
    // path as the pattern hijacked those calls into ast-grep (path classified
    // structural via its dots), silently emptying the agent's file listings and
    // flooding events/comparisons with junk rows that crowd real data out of
    // the report's recent-50 KPI window.

    #[test]
    fn files_mode_positional_is_a_path_not_a_pattern() {
        let inv = parse(&["--files", "/Users/x/.claude/plugins/cache"]);
        assert_eq!(inv.pattern, None);
        assert_eq!(inv.path, "/Users/x/.claude/plugins/cache");
    }

    #[test]
    fn files_mode_with_value_flags_still_pattern_less() {
        let inv = parse(&["--files", "-g", "*.md", "docs"]);
        assert_eq!(inv.pattern, None);
        assert_eq!(inv.path, "docs");
    }

    #[test]
    fn type_list_mode_is_pattern_less() {
        let inv = parse(&["--type-list"]);
        assert_eq!(inv.pattern, None);
    }

    // ── classify: path-like tokens are never structural ──

    #[test]
    fn absolute_path_is_not_structural() {
        assert!(!classify("/Users/user/.claude/plugins/cache"));
    }

    #[test]
    fn relative_path_is_not_structural() {
        assert!(!classify("src/main.rs"));
    }

    #[test]
    fn regex_with_slash_alternation_is_not_structural() {
        assert!(!classify("from.*invites/(AgentPicker|ModeSelector)"));
    }

    #[test]
    fn real_structural_patterns_still_classify() {
        assert!(classify("useState("));
        assert!(classify("Command::new("));
        assert!(classify("verify_aud"));
    }

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
    fn a_leading_dot_literal_is_not_structural() {
        // `rg '\.env'` is a dotfile/extension text search. Escaping used to make
        // this safe by accident; the dot heuristic must now reject it explicitly.
        assert!(!classify(r"\.env"));
        assert!(!classify(r"\.gitignore"));
        assert!(!classify(r"\.log"));
    }

    #[test]
    fn a_trailing_dot_literal_is_not_structural() {
        assert!(!classify(r"env\."));
    }

    #[test]
    fn an_interior_dot_is_still_structural() {
        assert!(classify(r"app\.current_tenant_id"));
        assert!(classify("app.current_tenant_id"));
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

    // ── translate_pattern: definitions need a body in brace languages ──

    // A definition translates to the bare `keyword name` signature — NOT a
    // paren/body form. ast-grep matches the whole function item from the signature
    // prefix regardless of return type (`-> u64`, `: number`, `error`) or body,
    // which a `name($$$) { $$$ }` pattern does NOT (it misses every fn with a
    // return type). Bare-name is the form that's robust without per-language churn.
    #[test]
    fn rust_fn_definition_becomes_bare_signature() {
        assert_eq!(translate_pattern("fn main("), "fn main");
    }

    #[test]
    fn ts_function_definition_becomes_bare_signature() {
        assert_eq!(translate_pattern("function useEffect("), "function useEffect");
    }

    #[test]
    fn go_func_definition_becomes_bare_signature() {
        assert_eq!(translate_pattern("func Handler("), "func Handler");
    }

    #[test]
    fn rust_fn_with_modifiers_keeps_them() {
        assert_eq!(translate_pattern("pub async fn run("), "pub async fn run");
    }

    #[test]
    fn call_expressions_stay_call_form() {
        assert_eq!(translate_pattern("useState("), "useState($$$)");
        assert_eq!(translate_pattern("Command::new("), "Command::new($$$)");
    }

    #[test]
    fn python_def_stays_paren_only_no_braces() {
        // Python is not a brace language; ast-grep matches `def foo($$$)` directly.
        assert_eq!(translate_pattern("def foo("), "def foo($$$)");
    }

    #[test]
    fn call_to_thing_named_func_is_not_a_definition() {
        // `func(` with no name after the keyword is a CALL, not a definition.
        assert_eq!(translate_pattern("func("), "func($$$)");
    }

    // ── flag fidelity: what ast-grep cannot express must pass through ──
    //
    // run_ast_grep honoured only -c/-l and silently dropped everything else, so a
    // redirected search answered a DIFFERENT question than the one asked — most
    // starkly `-v`, which returned the matching lines instead of the non-matching
    // ones. A flag we cannot honour must send the whole call to real rg.

    #[test]
    fn invert_match_is_unsupported() {
        assert_eq!(parse(&["-v", "deviceId", "src"]).unsupported.as_deref(), Some("-v"));
        assert_eq!(parse(&["--invert-match", "deviceId"]).unsupported.as_deref(), Some("--invert-match"));
    }

    #[test]
    fn context_flags_are_unsupported() {
        assert_eq!(parse(&["-A2", "deviceId", "src"]).unsupported.as_deref(), Some("-A"));
        assert_eq!(parse(&["-C", "2", "deviceId"]).unsupported.as_deref(), Some("-C"));
        assert_eq!(parse(&["--context=2", "deviceId"]).unsupported.as_deref(), Some("--context"));
    }

    #[test]
    fn case_insensitivity_is_unsupported_because_ast_grep_is_case_sensitive() {
        assert_eq!(parse(&["-i", "deviceId", "src"]).unsupported.as_deref(), Some("-i"));
        assert_eq!(parse(&["-S", "deviceId"]).unsupported.as_deref(), Some("-S"));
    }

    #[test]
    fn max_count_and_only_matching_are_unsupported() {
        assert_eq!(parse(&["-m1", "deviceId"]).unsupported.as_deref(), Some("-m"));
        assert_eq!(parse(&["-o", "deviceId"]).unsupported.as_deref(), Some("-o"));
    }

    #[test]
    fn unsupported_short_flag_inside_a_bundle_is_detected() {
        // `-nv` bundles line-numbers with invert; the invert must still be caught.
        assert_eq!(parse(&["-nv", "deviceId"]).unsupported.as_deref(), Some("-v"));
    }

    #[test]
    fn ordinary_flags_are_not_marked_unsupported() {
        assert_eq!(parse(&["-n", "-l", "--no-heading", "--color", "never", "deviceId", "src"]).unsupported, None);
        assert_eq!(parse(&["-c", "--type", "ts", "deviceId"]).unsupported, None);
    }

    #[test]
    fn a_flags_value_is_never_mistaken_for_a_flag() {
        // `-e -v` means "search for the literal -v", not "invert".
        let inv = parse(&["-e", "-v", "src"]);
        assert_eq!(inv.pattern.as_deref(), Some("-v"));
        assert_eq!(inv.unsupported, None);
    }

    #[test]
    fn nothing_after_double_dash_counts_as_a_flag() {
        let inv = parse(&["--", "-v", "src"]);
        assert_eq!(inv.pattern.as_deref(), Some("-v"));
        assert_eq!(inv.unsupported, None);
    }

    // ── flag fidelity: a call with more than one pattern SOURCE must pass
    // through, not be filtered against only the first ──
    //
    // `explicit_pattern` keeps exactly one -e/--regexp and discards the rest,
    // which used to be harmless (the kept pattern only decided WHETHER to
    // redirect; a wrong guess fell through to real rg). Once capture_argv
    // started forwarding EVERY pattern token to the rg capture, rg started
    // answering the UNION query while filter_matches kept suppressing
    // everything that didn't match the ONE retained pattern — silently
    // deleting real hits. -f/--file has the same defect from a different
    // angle: its argument is a FILE OF PATTERNS, not a pattern, so the
    // parser was filtering against the patterns file's own NAME.

    #[test]
    fn multiple_patterns_force_passthrough() {
        // rg answers the UNION of every -e; ast-grep takes one pattern, so it
        // would filter against only the first and delete the rest's real hits.
        assert!(parse(&["-e", "alphaCall\\(", "-e", "betaCall\\(", "f.py"]).unsupported.is_some());
        assert!(parse(&["--regexp", "a", "--regexp", "b", "f.py"]).unsupported.is_some());
    }

    #[test]
    fn a_single_pattern_still_redirects() {
        assert_eq!(parse(&["-e", "alphaCall\\(", "f.py"]).unsupported, None);
    }

    #[test]
    fn a_patterns_file_forces_passthrough() {
        // -f's argument is a file OF patterns. The parser stored the FILENAME as
        // the pattern, so the shim filtered against "queries.txt" itself.
        assert!(parse(&["-f", "queries.txt", "b.js"]).unsupported.is_some());
        assert!(parse(&["--file", "queries.txt", "b.js"]).unsupported.is_some());
    }

    #[test]
    fn a_patterns_file_still_consumes_its_value() {
        // Regression guard: -f must stay in short_takes_value, or "queries.txt"
        // becomes a positional and then the pattern.
        let inv = parse(&["-f", "queries.txt", "b.js"]);
        assert_eq!(inv.paths, strs(&["b.js"]), "queries.txt is -f's value, not a path");
    }

    // ── flag fidelity: what ast-grep CAN express must be forwarded ──
    //
    // `ast-grep run` accepts multiple [PATHS] and a --globs flag with gitignore
    // (`!`-negating) semantics, so these are forwarded rather than denied.

    #[test]
    fn every_positional_path_is_captured_not_just_the_first() {
        let inv = parse(&["deviceId", "src", "other"]);
        assert_eq!(inv.paths, vec!["src".to_string(), "other".to_string()]);
        assert_eq!(inv.path, "src", "path stays the first path for language inference");
    }

    #[test]
    fn stdin_dash_is_not_collected_as_a_path() {
        assert_eq!(parse(&["deviceId", "-", "src"]).paths, vec!["src".to_string()]);
    }

    #[test]
    fn globs_are_captured_for_forwarding() {
        let inv = parse(&["-g", "*.ts", "--glob=!test/**", "deviceId", "src"]);
        assert_eq!(inv.globs, vec!["*.ts".to_string(), "!test/**".to_string()]);
        assert_eq!(inv.unsupported, None, "globs are forwarded, not a passthrough reason");
        assert_eq!(inv.pattern.as_deref(), Some("deviceId"));
    }

    // ── output fidelity: line numbers and -c format must match ripgrep ──

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
    fn a_dot_slash_rg_path_matches_ast_greps_normalised_path() {
        // rg reports "./src/a.ts"; ast-grep reports "src/a.ts" for the same file.
        // Raw-string keying missed on every file for the `./src` call shape.
        let ag = parse_ag_matches(&ag_json("src/a.ts", 0, "const deviceId = 1;"));
        let spans = confirmed_spans(&ag);
        let hit = RgMatch {
            file: "./src/a.ts".into(),
            line: 1,
            text: "const deviceId = 1;".into(),
        };
        assert!(is_confirmed(&hit, &spans), "leading ./ must not defeat the lookup");
    }

    #[test]
    fn norm_path_strips_repeated_leading_dot_slash() {
        assert_eq!(norm_path("./src/a.ts"), "src/a.ts");
        assert_eq!(norm_path("././src/a.ts"), "src/a.ts");
        assert_eq!(norm_path("src/a.ts"), "src/a.ts");
        assert_eq!(norm_path("/abs/src/a.ts"), "/abs/src/a.ts");
    }

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

    #[test]
    fn ast_grep_line_zero_becomes_ripgrep_line_one() {
        // ast-grep's JSON range.start.line is 0-indexed; ripgrep reports 1-indexed.
        // Printing it raw pointed every redirected hit one line above the match.
        let m = parse_ag_matches(&ag_json("src/a.ts", 0, "const deviceId = 1;"));
        assert_eq!(m[0].line, 1);
        assert_eq!(m[0].file, "src/a.ts");
    }

    // ── filter_matches: rg is ground truth, ast-grep filters it ──

    fn rg_hit(file: &str, line: u64, text: &str) -> RgMatch {
        RgMatch { file: file.into(), line, text: text.into() }
    }

    fn searched_set(files: &[&str]) -> HashSet<String> {
        files.iter().map(|f| norm_path(f).to_string()).collect()
    }

    // ── never filter a file ast-grep did not search ──
    //
    // group_files_by_lang drops any extension it cannot map (.sql, .md, .json,
    // and case-mismatched .TS). Those files never reach ast-grep, so they can
    // produce no confirmations. Suppressing them would silently delete real
    // results — the failure class behind v0.3.9, v0.3.10 and v0.3.12.

    #[test]
    fn hits_in_an_unsearched_file_are_never_suppressed() {
        let hits = vec![
            rg_hit("db.py", 1, "bypass_rls(conn)"),
            rg_hit("db.py", 7, "# bypass_rls in a comment"),
            rg_hit("schema.sql", 3, "-- bypass_rls policy"),
        ];
        let ag = parse_ag_matches(&ag_json("db.py", 0, "bypass_rls(conn)"));
        let searched = searched_set(&["db.py"]); // schema.sql has no ast-grep language
        let out = filter_matches(&hits, &ag, &searched);
        assert_eq!(
            out.kept,
            vec![rg_hit("db.py", 1, "bypass_rls(conn)"), rg_hit("schema.sql", 3, "-- bypass_rls policy")],
            "the SQL hit must survive: ast-grep never looked at that file"
        );
        assert_eq!(out.suppressed, 1, "only the searched-but-unconfirmed comment");
    }

    #[test]
    fn filter_keeps_confirmed_hits_and_counts_the_rest() {
        let hits = vec![
            rg_hit("a.py", 1, "bypass_rls(conn)"),
            rg_hit("a.py", 7, "# bypass_rls is used above"),
            rg_hit("a.py", 9, "SQL = 'select bypass_rls'"),
        ];
        let ag = parse_ag_matches(&ag_json("a.py", 0, "bypass_rls(conn)"));
        let out = filter_matches(&hits, &ag, &searched_set(&["a.py"]));
        assert_eq!(out.kept, vec![rg_hit("a.py", 1, "bypass_rls(conn)")]);
        assert_eq!(out.suppressed, 2, "the comment and the SQL string");
    }

    #[test]
    fn filter_with_no_confirmations_keeps_nothing_from_searched_files() {
        let hits = vec![rg_hit("a.py", 1, "tenant_id = 1")];
        let out = filter_matches(&hits, &[], &searched_set(&["a.py"]));
        assert!(out.kept.is_empty());
        assert_eq!(out.suppressed, 1);
    }

    #[test]
    fn a_dot_slash_hit_matches_a_normalised_searched_path() {
        // rg reports "./db.py"; the searched set is built with norm_path.
        let hits = vec![rg_hit("./db.py", 1, "bypass_rls(conn)")];
        let ag = parse_ag_matches(&ag_json("db.py", 0, "bypass_rls(conn)"));
        let out = filter_matches(&hits, &ag, &searched_set(&["db.py"]));
        assert_eq!(out.kept.len(), 1, "leading ./ must not defeat the searched lookup");
        assert_eq!(out.suppressed, 0);
    }

    #[test]
    fn a_language_that_confirms_nothing_is_treated_as_unsearched() {
        // ast-grep against the wrong grammar exits 1 with no output AND no
        // stderr, so "failed" and "found nothing" are indistinguishable. A real
        // JS call inside a .html <script> block was being suppressed as noise.
        let hits = vec![
            rg_hit("app.py", 2, "    render_chart(data)"),
            rg_hit("page.html", 2, "  render_chart(data);"),
        ];
        let ag = parse_ag_matches(&ag_json("app.py", 1, "    render_chart(data)"));
        // Only python confirmed, so only python is in `searched`.
        let out = filter_matches(&hits, &ag, &searched_set(&["app.py"]));
        assert_eq!(out.kept.len(), 2, "the .html hit must survive: html confirmed nothing");
        assert_eq!(out.suppressed, 0);
    }

    #[test]
    fn a_language_that_confirms_nothing_is_excluded_from_searched() {
        // The root-cause test for C1: a real JS call inside a .html file was
        // suppressed because ast-grep parsed it as HTML and confirmed nothing —
        // and a wrong-grammar run is indistinguishable from a genuine empty.
        let mut by_lang: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        by_lang.insert("python", strs(&["app.py"]));
        by_lang.insert("html", strs(&["page.html"]));

        // python confirms; html returns nothing, as a wrong-grammar run does.
        let (ag, searched, confirming) = confirm_by_language(&by_lang, |lang, _files| {
            if lang == "python" {
                parse_ag_matches(&ag_json("app.py", 1, "    render_chart(d)"))
            } else {
                Vec::new()
            }
        });

        assert_eq!(ag.len(), 1);
        assert!(searched.contains("app.py"));
        assert!(!searched.contains("page.html"), "html confirmed nothing => not searched");
        assert_eq!(confirming, vec!["python"], "only confirming languages are credited");
    }

    // ── the comparison metric covers SEARCHED files only ──

    #[test]
    fn comparison_counts_exclude_unsearched_files() {
        // Counting unsearched hits as rg_results while ast-grep never saw them
        // would manufacture fake "noise avoided".
        let hits = vec![
            rg_hit("a.py", 1, "bypass_rls(conn)"),
            rg_hit("a.py", 7, "# bypass_rls"),
            rg_hit("notes.md", 2, "bypass_rls docs"),
        ];
        let ag = parse_ag_matches(&ag_json("a.py", 0, "bypass_rls(conn)"));
        let out = filter_matches(&hits, &ag, &searched_set(&["a.py"]));
        assert_eq!(out.searched_hits, 2, "only the two a.py hits");
        assert_eq!(out.confirmed_hits, 1);
    }

    // ── render_output now renders ripgrep hits ──

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

    // ── report totals cover the whole table, not the displayed page ──
    //
    // The headline KPIs were summed inside the `ORDER BY id DESC LIMIT 50`
    // query that feeds the detail TABLE, so "Comparison Runs" read exactly 50
    // for any database with 50+ rows, and every savings figure described only
    // the newest 50 rows while the counters beside them were all-time. A live
    // DB with 402 rows reported 104 files saved instead of 4,748.

    fn seed_comparisons(conn: &Connection, rows: usize) {
        for i in 0..rows {
            conn.execute(
                "INSERT INTO comparisons (pattern, lang, ag_matches, ag_files, ag_time_ms,
                     rg_results, rg_files, rg_time_ms, files_saved, estimated_tokens_saved,
                     estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents,
                     ast_cost_cents, ts)
                 VALUES ('p', 'typescript', 2, 1, 1, 5, 2, 1, 1, 45, 0.9, 75, 30, 1.5, 0.6, ?1)",
                rusqlite::params![format!("{}.000Z", 1_750_000_000u64 + i as u64)],
            )
            .unwrap();
        }
    }

    fn memory_db(rows: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_comparisons(&conn, rows);
        conn
    }

    #[test]
    fn comparison_runs_counts_every_row_not_just_the_displayed_page() {
        let totals = comparison_totals(&memory_db(60));
        assert_eq!(totals.runs, 60, "must not saturate at the 50-row display page");
    }

    #[test]
    fn savings_totals_cover_every_row_not_just_the_displayed_page() {
        let totals = comparison_totals(&memory_db(60));
        assert_eq!(totals.files_saved, 60);
        assert_eq!(totals.false_positives, 180, "60 rows x (5 rg - 2 ag)");
        assert_eq!(totals.tokens_saved, 2700, "60 rows x 45");
        assert!((totals.cost_saved_cents - 54.0).abs() < 1e-9, "60 rows x 0.9c");
    }

    #[test]
    fn totals_fall_back_to_text_minus_ast_when_no_estimate_is_stored() {
        // Legacy/seeded rows carry 0 in the estimate columns but real
        // text_/ast_ figures; the totals must mirror the table's precedence.
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        conn.execute(
            "INSERT INTO comparisons (pattern, lang, ag_matches, ag_files, ag_time_ms,
                 rg_results, rg_files, rg_time_ms, files_saved, estimated_tokens_saved,
                 estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents,
                 ast_cost_cents, ts)
             VALUES ('p','typescript',2,1,1,5,2,1,3,0,0.0,500,200,10.0,4.0,'1750000000.000Z')",
            [],
        )
        .unwrap();
        let totals = comparison_totals(&conn);
        assert_eq!(totals.tokens_saved, 300, "text 500 - ast 200");
        assert!((totals.cost_saved_cents - 6.0).abs() < 1e-9, "10.0c - 4.0c");
    }

    #[test]
    fn a_negative_saving_is_floored_at_zero_never_reported_as_a_loss() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        conn.execute(
            "INSERT INTO comparisons (pattern, lang, ag_matches, ag_files, ag_time_ms,
                 rg_results, rg_files, rg_time_ms, files_saved, estimated_tokens_saved,
                 estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents,
                 ast_cost_cents, ts)
             VALUES ('p','typescript',9,1,1,2,1,1,0,0,0.0,30,135,0.6,2.7,'1750000000.000Z')",
            [],
        )
        .unwrap();
        let totals = comparison_totals(&conn);
        assert_eq!(totals.false_positives, 0, "ag > rg is not negative noise");
        assert_eq!(totals.tokens_saved, 0);
        assert_eq!(totals.cost_saved_cents, 0.0);
    }

    #[test]
    fn empty_table_totals_are_zero() {
        let totals = comparison_totals(&memory_db(0));
        assert_eq!(totals.runs, 0);
        assert_eq!(totals.files_saved, 0);
        assert_eq!(totals.tokens_saved, 0);
    }

    // ── I1: events.lang is a comma-joined list of every CONFIRMING language,
    // not always one — aggregating on the raw string fragments the KPI it
    // feeds ──
    //
    // log_event's lang_label (main(), the confirming_langs.join(",") call)
    // joins every language that actually confirmed a hit for a genuinely
    // polyglot search. A naive `GROUP BY lang` then treats "python,typescript"
    // as its own bucket, distinct from "python" and "typescript" — the more
    // successful the polyglot fix is, the more it fragments its own metric.

    fn seed_event(conn: &Connection, event: &str, pattern: &str, lang: &str, matches: u64) {
        conn.execute(
            "INSERT INTO events (agent, event, pattern, reason, lang, matches, ts)
             VALUES ('claude-code', ?1, ?2, 'filtered', ?3, ?4, '1750000000.000Z')",
            rusqlite::params![event, pattern, lang, matches],
        )
        .unwrap();
    }

    #[test]
    fn polyglot_lang_label_is_split_across_its_languages_not_its_own_bucket() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        // One polyglot redirect (both langs confirmed in the same event) plus
        // two python-only redirects.
        seed_event(&conn, "structural", "armingSnapshot($$$)", "python,typescript", 2);
        seed_event(&conn, "structural", "otherCall($$$)", "python", 3);
        seed_event(&conn, "structural", "thirdCall($$$)", "python", 1);

        let counts = language_counts(&conn);
        assert_eq!(counts.get("python"), Some(&3), "2 python-only events + 1 from the polyglot row");
        assert_eq!(counts.get("typescript"), Some(&1), "only the polyglot row touched typescript");
        assert_eq!(counts.get("python,typescript"), None, "must not survive as its own bucket");
    }

    #[test]
    fn top_patterns_also_splits_the_comma_joined_lang() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_event(&conn, "structural", "armingSnapshot($$$)", "python,typescript", 1);
        seed_event(&conn, "structural", "armingSnapshot($$$)", "python", 1);

        let top = top_pattern_stats(&conn);
        let python_count: u64 = top
            .iter()
            .filter(|p| p.pattern == "armingSnapshot($$$)" && p.lang == "python")
            .map(|p| p.count)
            .sum();
        assert_eq!(python_count, 2, "the polyglot row's count must fold into python too");
        assert!(top.iter().all(|p| p.lang != "python,typescript"), "no composite bucket");
    }

    // ── I2: no_match (rg found nothing, ast-grep never ran) is neither a win
    // nor a failure — it must not sit in the redirect-rate denominator, and
    // it needs its own series rather than being folded into "Text" ──

    #[test]
    fn redirect_rate_excludes_no_match_from_the_denominator() {
        // 1 structural win, 3 pure no-match searches: 100% of the searches
        // that had anything TO redirect were in fact redirected.
        assert_eq!(redirect_rate(4, 1, 3), 100.0);
    }

    #[test]
    fn redirect_rate_is_zero_when_everything_is_a_no_match() {
        assert_eq!(redirect_rate(3, 0, 3), 0.0, "0/0 after excluding no_match must not divide by zero");
    }

    #[test]
    fn day_counts_carries_no_match_as_its_own_column() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn);
        seed_event(&conn, "structural", "p", "python", 1);
        seed_event(&conn, "no_match", "p2", "", 0);
        seed_event(&conn, "no_match", "p3", "", 0);

        let days = day_counts(&conn);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].total, 3);
        assert_eq!(days[0].structural, 1);
        assert_eq!(days[0].no_match, 2);
    }

    #[test]
    fn line_number_preference_is_tracked() {
        assert_eq!(parse(&["-n", "deviceId", "src"]).line_numbers, Some(true));
        assert_eq!(parse(&["--line-number", "deviceId"]).line_numbers, Some(true));
        assert_eq!(parse(&["-N", "deviceId"]).line_numbers, Some(false));
        assert_eq!(parse(&["--no-line-number", "deviceId"]).line_numbers, Some(false));
        // Unset → rg's own default applies (on for a TTY, off when piped).
        assert_eq!(parse(&["deviceId", "src"]).line_numbers, None);
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
        // `rg -c PATTERN src/a.ts` prints a bare count, no path prefix.
        let m = vec![rg_hit("src/a.ts", 1, "a"), rg_hit("src/a.ts", 5, "b")];
        assert_eq!(render_output(&m, OutputMode::Count { show_filename: false }), "2\n");
    }

    #[test]
    fn files_with_matches_output_is_sorted_and_deduped() {
        let m = vec![rg_hit("src/b.ts", 1, "a"), rg_hit("src/a.ts", 1, "b"), rg_hit("src/b.ts", 4, "c")];
        assert_eq!(render_output(&m, OutputMode::FilesWithMatches), "src/a.ts\nsrc/b.ts\n");
    }

    // ── ripgrep --json parsing ──

    fn rg_json_match(file: &str, line: u64, text: &str) -> String {
        format!(
            r#"{{"type":"match","data":{{"path":{{"text":"{file}"}},"lines":{{"text":"{text}\n"}},"line_number":{line},"absolute_offset":0,"submatches":[]}}}}"#
        )
    }

    #[test]
    fn parses_a_match_record_with_one_based_line() {
        let (m, _) = parse_rg_json(&rg_json_match("src/a.ts", 1, "const deviceId = 1;"));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].file, "src/a.ts");
        assert_eq!(m[0].line, 1, "ripgrep line_number is already 1-based");
        assert_eq!(m[0].text, "const deviceId = 1;", "trailing newline stripped");
    }

    #[test]
    fn parse_rg_json_reports_records_it_cannot_parse() {
        // rg emits lines.bytes (base64) for a non-UTF-8 line. That record holds
        // a real hit we cannot represent, so it must be COUNTED, not dropped.
        let stream = format!(
            "{}\n{}",
            rg_json_match("a.py", 2, "render_chart(b)"),
            r#"{"type":"match","data":{"path":{"text":"a.py"},"lines":{"bytes":"cmVuZGVy"},"line_number":1,"absolute_offset":0,"submatches":[]}}"#
        );
        let (matches, unparseable) = parse_rg_json(&stream);
        assert_eq!(matches.len(), 1);
        assert_eq!(unparseable, 1, "the bytes-only record must be counted");
    }

    #[test]
    fn parse_rg_json_reports_zero_unparseable_for_clean_input() {
        let (matches, unparseable) = parse_rg_json(&rg_json_match("a.py", 1, "hit"));
        assert_eq!(matches.len(), 1);
        assert_eq!(unparseable, 0);
    }

    #[test]
    fn ignores_non_match_records() {
        let stream = format!(
            "{}\n{}\n{}",
            r#"{"type":"begin","data":{"path":{"text":"src/a.ts"}}}"#,
            rg_json_match("src/a.ts", 3, "hit"),
            r#"{"type":"end","data":{"path":{"text":"src/a.ts"}}}"#
        );
        let (m, _) = parse_rg_json(&stream);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 3);
    }

    #[test]
    fn empty_stream_yields_no_matches() {
        assert!(parse_rg_json("").0.is_empty());
    }

    #[test]
    fn crlf_line_endings_leave_no_stray_carriage_return() {
        // Real `rg --json` emits "text\r\n" for a CRLF-terminated line. Stripping
        // only '\n' baked a '\r' into every match from a Windows-authored file.
        let stream = format!(
            r#"{{"type":"match","data":{{"path":{{"text":"win.ts"}},"lines":{{"text":"const deviceId = 1;\r\n"}},"line_number":1,"absolute_offset":0,"submatches":[]}}}}"#
        );
        let (m, _) = parse_rg_json(&stream);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "const deviceId = 1;", "no trailing \\r");
    }

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
        let inv = parse(&["-n", "--no-smart", "deviceId", "src"]);
        let args = strs(&["-n", "--no-smart", "deviceId", "src"]);
        assert_eq!(
            strip_shim_flags(&args, &inv.shim_flag_indices),
            strs(&["-n", "deviceId", "src"]),
            "rg would reject --no-smart"
        );
    }

    #[test]
    fn no_smart_after_double_dash_is_a_pattern_not_a_flag() {
        // `rg -- --no-smart` searches for that literal text. Stripping it left a
        // bare `rg --`, which fails with "required argument <PATTERN>".
        let inv = parse(&["--", "--no-smart", "src"]);
        assert_eq!(inv.pattern.as_deref(), Some("--no-smart"));
        assert!(!inv.no_smart);
        let args = strs(&["--", "--no-smart", "src"]);
        assert_eq!(strip_shim_flags(&args, &inv.shim_flag_indices), args,
            "nothing after -- may be stripped");
    }

    #[test]
    fn no_smart_as_a_flag_value_is_not_stripped() {
        // `-e --no-smart src` searches for that literal in src. Stripping the
        // value silently turned this into "search for src in the cwd".
        let inv = parse(&["-e", "--no-smart", "src"]);
        assert_eq!(inv.pattern.as_deref(), Some("--no-smart"));
        assert!(!inv.no_smart);
        let args = strs(&["-e", "--no-smart", "src"]);
        assert_eq!(strip_shim_flags(&args, &inv.shim_flag_indices), args,
            "-e's value must survive");
    }

    #[test]
    fn no_smart_with_an_inline_value_is_stripped() {
        let inv = parse(&["--no-smart=true", "deviceId", "src"]);
        assert!(inv.no_smart);
        let args = strs(&["--no-smart=true", "deviceId", "src"]);
        assert_eq!(strip_shim_flags(&args, &inv.shim_flag_indices),
            strs(&["deviceId", "src"]), "the =value spelling must strip too");
    }

    // ── language comes from the files that MATCHED ──

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

    // ── capturing rg: capture_argv is a byproduct of the parser's own walk ──

    #[test]
    fn capture_argv_strips_output_modes_and_keeps_filters() {
        let inv = parse(&["-c", "-n", "--heading", "-g", "*.ts", "deviceId", "src"]);
        for stripped in ["-c", "-n", "--heading"] {
            assert!(!inv.capture_argv.contains(&stripped.to_string()), "{stripped} must be stripped");
        }
        // Filters and positionals survive, in order.
        assert!(inv.capture_argv.windows(2).any(|w| w == ["-g".to_string(), "*.ts".to_string()]));
        assert!(inv.capture_argv.contains(&"deviceId".to_string()));
        assert!(inv.capture_argv.contains(&"src".to_string()));
    }

    #[test]
    fn capture_argv_keeps_the_type_filter_adjacent_to_its_value() {
        let inv = parse(&["--type", "ts", "deviceId", "src"]);
        assert!(inv.capture_argv.windows(2).any(|w| w == ["--type".to_string(), "ts".to_string()]));
    }

    #[test]
    fn capture_argv_strips_bundled_short_output_flags() {
        // `-nc` is ordinary ripgrep usage. Exact-token matching missed it, and
        // rg's mode precedence let -c beat --json, so the capture came back as
        // plain text and parsed to a silent false-empty.
        let inv = parse(&["-nc", "deviceId", "src"]);
        assert!(!inv.capture_argv.iter().any(|a| a == "-nc"), "bundled -nc must be stripped");
        assert!(inv.capture_argv.contains(&"deviceId".to_string()));
        assert!(inv.capture_argv.contains(&"src".to_string()));
    }

    #[test]
    fn capture_argv_strips_bundled_cl() {
        let inv = parse(&["-cl", "deviceId", "src"]);
        assert!(!inv.capture_argv.iter().any(|a| a == "-cl"), "bundled -cl must be stripped");
    }

    #[test]
    fn capture_argv_keeps_a_bare_dash_stdin_marker() {
        let inv = parse(&["deviceId", "-"]);
        assert!(inv.capture_argv.contains(&"-".to_string()), "a bare - is stdin, not a flag");
    }

    #[test]
    fn vimgrep_and_passthru_force_passthrough() {
        // --vimgrep wants file:line:COLUMN:text, which the shim does not emit.
        // --passthru wants every line; under --json those are `context` records
        // that parse_rg_json discards. Both are correct only via real rg.
        assert_eq!(parse(&["--vimgrep", "deviceId", "src"]).unsupported.as_deref(), Some("--vimgrep"));
        assert_eq!(parse(&["--passthru", "deviceId", "src"]).unsupported.as_deref(), Some("--passthru"));
    }

    #[test]
    fn capture_argv_keeps_a_bundled_type_value() {
        // `-tcpp` is `--type cpp`. Scanning every character saw the 'c' in the
        // VALUE and dropped the caller's type filter entirely.
        let inv = parse(&["-tcpp", "deviceId", "src"]);
        assert!(inv.capture_argv.contains(&"-tcpp".to_string()), "-t's bundled value must survive");
    }

    #[test]
    fn capture_argv_keeps_a_bundled_glob_value() {
        let inv = parse(&["-g*.c", "deviceId", "src"]);
        assert!(inv.capture_argv.contains(&"-g*.c".to_string()), "-g's bundled value must survive");
    }

    #[test]
    fn capture_argv_keeps_a_bundled_pattern_value() {
        // `-eDeviceId` carries the SEARCH TERM. Dropping it made ripgrep treat
        // the path as the pattern — a silent wrong answer.
        let inv = parse(&["-eDeviceId", "src"]);
        assert!(inv.capture_argv.contains(&"-eDeviceId".to_string()), "-e's bundled pattern must survive");
    }

    #[test]
    fn capture_argv_still_strips_output_mode_before_a_value_flag() {
        // `-cg` is count bundled ahead of a glob: the 'c' comes first, so the
        // token is rewritten to just `-g` (output mode dropped, filter kept).
        let inv = parse(&["-cg", "*.ts", "deviceId", "src"]);
        assert!(!inv.capture_argv.iter().any(|a| a == "-cg"), "bundled -cg token must be rewritten");
        assert!(inv.capture_argv.contains(&"-g".to_string()), "-g must survive so *.ts stays its value");
        assert!(inv.capture_argv.contains(&"*.ts".to_string()));
    }

    #[test]
    fn capture_argv_keeps_the_glob_flag_from_a_mixed_bundle() {
        // `-cg *.ts` is count + glob whose value is the NEXT token. Dropping the
        // whole token orphaned "*.ts" as a bare positional, shifting ripgrep's
        // pattern/path split and silently changing what is searched.
        let inv = parse(&["-cg", "*.ts", "deviceId", "src"]);
        assert!(inv.capture_argv.contains(&"-g".to_string()), "-g must survive so *.ts stays its value");
        assert!(!inv.capture_argv.iter().any(|a| a == "-cg"));
        assert!(inv.capture_argv.contains(&"*.ts".to_string()));
    }

    #[test]
    fn capture_argv_keeps_an_inline_value_from_a_mixed_bundle() {
        // `-ntrust` is -n bundled with `--type rust`. Rewriting keeps `-trust` so
        // the type filter is preserved.
        let inv = parse(&["-ntrust", "deviceId", "src"]);
        assert!(inv.capture_argv.contains(&"-trust".to_string()), "the type filter must survive");
    }

    #[test]
    fn capture_argv_keeps_the_pattern_flag_from_a_mixed_bundle() {
        // `src -ce DeviceId`: -e carries the PATTERN. Rewriting to `-e` preserves it.
        let inv = parse(&["src", "-ce", "DeviceId"]);
        assert_eq!(inv.capture_argv, strs(&["src", "-e", "DeviceId"]));
    }

    #[test]
    fn capture_argv_keeps_a_non_output_filter_from_a_bundle() {
        // -w is a word-boundary FILTER; only the -n may be removed.
        let inv = parse(&["-nw", "deviceId", "src"]);
        assert!(inv.capture_argv.contains(&"-w".to_string()), "-w must survive");
    }

    #[test]
    fn capture_argv_drops_a_token_that_is_only_output_modes() {
        let inv = parse(&["-nc", "deviceId", "src"]);
        assert!(!inv.capture_argv.iter().any(|a| a.starts_with('-')),
            "-nc is purely output modes and must vanish entirely");
    }

    // ── round 4: capture_argv is a byproduct of the parser's own walk ──

    #[test]
    fn capture_argv_treats_everything_after_double_dash_as_positional() {
        // `rg pattern -- -c` searches for `pattern` in a FILE named `-c`.
        let inv = parse(&["pattern", "--", "-c"]);
        assert_eq!(inv.capture_argv, strs(&["pattern", "--", "-c"]));
    }

    #[test]
    fn capture_argv_never_mangles_a_post_double_dash_filename() {
        // `-count.log` had its 'c' and 'n' stripped out of it.
        let inv = parse(&["pattern", "--", "-count.log"]);
        assert_eq!(inv.capture_argv, strs(&["pattern", "--", "-count.log"]));
    }

    #[test]
    fn capture_argv_does_not_reinterpret_a_dash_prefixed_flag_value() {
        // `rg -e -c src` searches for the literal text `-c` inside src/.
        // Re-running "-c" through flag classification dropped it, so rg took
        // "src" as the pattern and searched the cwd instead.
        let inv = parse(&["-e", "-c", "src"]);
        assert_eq!(inv.capture_argv, strs(&["-e", "-c", "src"]));
    }

    #[test]
    fn capture_argv_omits_the_shim_own_flag() {
        let inv = parse(&["--no-smart", "deviceId", "src"]);
        assert_eq!(inv.capture_argv, strs(&["deviceId", "src"]));
    }

    #[test]
    fn capture_command_args_prepends_json() {
        assert_eq!(
            capture_command_args(&strs(&["deviceId", "src"])),
            strs(&["--json", "deviceId", "src"])
        );
    }

    // ── round 5: ripgrep's own top-level modes, and the composed argv ──

    #[test]
    fn capture_argv_excludes_ripgreps_own_top_level_modes() {
        // `rg --json --files src` prints a plain file list and exits 0, so it
        // would parse to a silent false-empty. Excluded here so capture_argv
        // does not rely on main()'s gate.
        assert_eq!(parse(&["--files", "src"]).capture_argv, strs(&["src"]));
        assert!(parse(&["--type-list"]).capture_argv.is_empty());
    }

    #[test]
    fn capture_command_args_composes_with_the_parsers_argv() {
        let inv = parse(&["-c", "-n", "--heading", "-g", "*.ts", "deviceId", "src"]);
        assert_eq!(
            capture_command_args(&inv.capture_argv),
            strs(&["--json", "-g", "*.ts", "deviceId", "src"])
        );
    }
}
