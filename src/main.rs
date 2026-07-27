// smart-rg: A drop-in rg replacement that redirects structural code searches
// to ast-grep. Claude Code / Hermes / any coding agent compatible.
//
// Architecture:
//   Input (rg flags) → Classify pattern → Structural? → ast-grep → Reformat → Output
//                                       → Text?       → real rg  → Output
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
    // -n/--line-number → Some(true), -N/--no-line-number → Some(false), unset →
    // None, meaning rg's own default (on for a TTY, off when piped).
    line_numbers: Option<bool>,
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

fn parse_rg_invocation(args: &[String]) -> RgInvocation {
    let mut inv = RgInvocation { path: ".".into(), ..Default::default() };
    let mut positionals: Vec<String> = Vec::new();
    let mut explicit_pattern: Option<String> = None;
    let mut i = 0;

    while i < args.len() {
        let a = &args[i];

        // Everything after `--` is positional, verbatim.
        if a == "--" {
            positionals.extend(args[i + 1..].iter().cloned());
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
            if value.is_none() && LONG_VALUE_FLAGS.contains(&full.as_str()) && i + 1 < args.len() {
                value = Some(args[i + 1].clone());
                i += 1; // consume the value token
            }
            if UNSUPPORTED_LONG.contains(&name) && inv.unsupported.is_none() {
                inv.unsupported = Some(full.clone());
            }
            match name {
                "regexp" | "file" => { if explicit_pattern.is_none() { explicit_pattern = value; } }
                "type" => { if value.is_some() { inv.file_type = value; } }
                "glob" => { if let Some(v) = value { inv.globs.push(v); } }
                "count" => inv.count = true,
                "files-with-matches" => inv.files_with_matches = true,
                "files" | "type-list" => inv.pattern_less = true,
                "line-number" => inv.line_numbers = Some(true),
                "no-line-number" => inv.line_numbers = Some(false),
                _ => {}
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
                        'e' | 'f' => { if explicit_pattern.is_none() { explicit_pattern = Some(value); } }
                        't' => inv.file_type = Some(value),
                        'g' => inv.globs.push(value),
                        _ => {}
                    }
                    break; // the rest of the bundle is this flag's value
                }
                idx += 1;
            }
            if consumed_next { i += 1; }
            i += 1;
            continue;
        }

        // Positional (pattern or path).
        positionals.push(a.clone());
        i += 1;
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

    let pattern = match inv.pattern.as_ref() {
        Some(p) => p.clone(),
        // No search term (e.g. `--files`, `--version`, `--type-list`): forward as-is.
        None => exec_real_rg(&args[1..]),
    };

    // Stream-filter guard: `cmd | rg PATTERN` reads stdin, which ast-grep cannot
    // search. Forward verbatim so the pipe is filtered correctly instead of being
    // silently dropped while ast-grep searches the cwd.
    // Explicit `-` stdin OR an implicit pipe (no path + non-TTY stdin): ast-grep
    // has no stdin-search mode, so forward verbatim or the stream is dropped.
    if inv.reads_stdin || is_stream_filter(inv.has_path, std::io::stdin().is_terminal()) {
        let reason = if inv.reads_stdin { "stdin_dash" } else { "stream_stdin" };
        log_event("passthrough", &pattern, reason, None, 0);
        exec_real_rg(&args[1..]);
    }

    // Flag-fidelity guard: a flag whose semantics ast-grep cannot reproduce
    // (invert, context, case-folding, capping, …) would make a redirect answer a
    // DIFFERENT question than the caller asked. Forward the call verbatim so the
    // answer is right; the lost redirect is only a lost optimisation.
    if let Some(flag) = inv.unsupported.clone() {
        log_event("passthrough", &pattern, &format!("unsupported_flag_{flag}"), None, 0);
        exec_real_rg(&args[1..]);
    }

    let lang_from_type = map_lang(&inv.file_type);
    let is_structural = classify(&pattern);

    // If no --type flag, try to infer language from the search path's file extensions.
    let lang = lang_from_type.or_else(|| infer_lang_from_path(&inv.path));

    if !is_structural || lang.is_none() {
        let reason = if !is_structural { "not_structural" } else { "no_language" };
        log_event("passthrough", &pattern, reason, lang, 0);
        exec_real_rg(&args[1..]);
    }

    let lang = lang.unwrap();
    let sg_pattern = translate_pattern(&pattern);

    eprintln!("\x1b[36m🔀 smart-rg → ast-grep ({})  pattern: '{}'\x1b[0m", lang, sg_pattern);

    let match_count = run_ast_grep(&sg_pattern, lang, &inv.path, &inv);

    // run_ast_grep already handled genuine errors (non-empty stderr -> exec real
    // rg before returning), so here a clean run with 0 matches means ast-grep
    // found nothing — commonly a wrong-language guess over a polyglot tree. Fall
    // back to real rg so the user gets real hits instead of a silent empty.
    // Logged as `fallback`, never a structural win, so the noise-avoided metric
    // stays honest.
    if match_count == 0 {
        log_event("fallback", &pattern, "ast_grep_empty", Some(lang), 0);
        exec_real_rg(&args[1..]);
    }
    log_event("structural", &sg_pattern, "redirected", Some(lang), match_count);
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

fn map_lang(file_type: &Option<String>) -> Option<&str> {
    match file_type.as_deref() {
        Some("ts") | Some("typescript") => Some("typescript"),
        Some("tsx") => Some("tsx"),
        Some("js") | Some("javascript") => Some("javascript"),
        Some("jsx") => Some("jsx"),
        Some("py") | Some("python") => Some("python"),
        Some("rs") | Some("rust") => Some("rust"),
        Some("go") | Some("golang") => Some("go"),
        Some("rb") | Some("ruby") => Some("ruby"),
        Some("java") => Some("java"),
        Some("c") | Some("cpp") | Some("c++") => Some("c"),
        Some("css") => Some("css"),
        Some("html") => Some("html"),
        Some("swift") => Some("swift"),
        Some("kt") | Some("kotlin") => Some("kotlin"),
        Some("scala") => Some("scala"),
        Some("php") => Some("php"),
        Some("sql") => Some("sql"),
        Some("sh") | Some("bash") | Some("shell") => Some("bash"),
        _ => None,
    }
}

// Infer the dominant language by counting file extensions under a path.
// Only called when no --type flag was passed; returns None if path isn't a dir
// or has no recognizable source files. Capped at a shallow scan (max_depth=2)
// so it never blocks on large trees.
fn infer_lang_from_path(path: &str) -> Option<&'static str> {
    use std::collections::HashMap;
    let base = std::path::Path::new(path);
    if !base.is_dir() {
        // Single file — detect from extension directly.
        return ext_to_lang(base.extension()?.to_str()?);
    }

    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    // Walk up to depth 2 to keep this cheap.
    walk_for_lang(base, 0, &mut counts);

    dominant_lang(&counts)
}

// Choose the dominant language from extension counts. Two rules beyond "most
// frequent wins": (1) a real programming language always beats markup/style
// (html/css) — a stray `report.html` next to `main.rs` must not flip a Rust dir
// to HTML; (2) ties resolve alphabetically so the result is deterministic (a
// HashMap's iteration order is not).
fn dominant_lang(counts: &std::collections::HashMap<&'static str, usize>) -> Option<&'static str> {
    const MARKUP: &[&str] = &["html", "css"];
    // Within a group, pick the highest count; break ties by the alphabetically
    // smaller name (then.cmp reversed) so the choice is deterministic.
    let best = |markup_group: bool| -> Option<&'static str> {
        counts.iter()
            .filter(|(lang, _)| MARKUP.contains(lang) == markup_group)
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(lang, _)| *lang)
    };
    // Programming languages first; fall back to markup only if none present.
    best(false).or_else(|| best(true))
}

fn walk_for_lang(dir: &std::path::Path, depth: u32, counts: &mut std::collections::HashMap<&'static str, usize>) {
    if depth > 2 { return; }
    let rd = match std::fs::read_dir(dir) { Ok(r) => r, Err(_) => return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "node_modules" || name == "target" { continue; }
            walk_for_lang(&p, depth + 1, counts);
        } else if let Some(lang) = p.extension().and_then(|e| e.to_str()).and_then(ext_to_lang) {
            *counts.entry(lang).or_insert(0) += 1;
        }
    }
}

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

// ── run_rg_count (for comparison baseline using --count) ────
/// Runs real rg --count on the ORIGINAL user CLI args (replay) to get
/// accurate (total_matches, file_count) for ROI savings vs AST results.
fn run_rg_count(original_args: &[String], search_path: &str) -> (u64, u64) {
    // Resolve the real ripgrep safely (never the shim — see real_rg_path).
    // If we can't find it, skip the baseline rather than risk a re-exec loop.
    let rg = match real_rg_path() {
        Some(p) => p,
        None => return (0, 0),
    };

    let mut cmd = Command::new(&rg);
    // --count-matches, NOT --count: ast-grep's figure is a count of matched AST
    // NODES (occurrences), while `--count` reports the number of matching LINES.
    // Comparing the two measured different things — two occurrences on one line
    // scored rg=1 against ag=2, producing "impossible" rows where the text tool
    // appears to find LESS than the structural one and systematically
    // understating the noise ast-grep avoided.
    //
    // -F (literal): the intercepted pattern is often a structural form like `foo(`
    // that is an INVALID regex for ripgrep. Without -F the baseline silently errors
    // to 0 results and the reported savings collapse for exactly the paren-style
    // patterns the shim is built to redirect.
    cmd.arg("--count-matches").arg("-F");
    let mut i = 0;
    let args_slice = original_args;
    while i < args_slice.len() {
        let arg = &args_slice[i];
        // Skip output-mode flags that conflict with --count
        if matches!(arg.as_str(), "-c" | "--count" | "--count-matches" | "-l" | "--files-with-matches"
            | "--files" | "--files-without-match" | "-o" | "--only-matching") {
            i += 1;
            continue;
        }
        // Translate --type to -g globs for EVERY language map_lang accepts. We
        // glob uniformly rather than pass some type names through, because rg's
        // type vocabulary doesn't match ast-grep's: rg has no `tsx`/`jsx` type and
        // names Rust/Ruby `rust`/`ruby`, so `--type rs` etc. would error and zero
        // the baseline. Globs always work; unknown types fall through unchanged.
        if arg == "--type" || arg == "-t" {
            if i + 1 < args_slice.len() {
                let globs: &[&str] = match args_slice[i + 1].as_str() {
                    "ts" | "typescript" => &["*.ts"],
                    "tsx" => &["*.tsx"],
                    "js" | "javascript" => &["*.js"],
                    "jsx" => &["*.jsx"],
                    "py" | "python" => &["*.py"],
                    "rs" | "rust" => &["*.rs"],
                    "rb" | "ruby" => &["*.rb"],
                    "go" | "golang" => &["*.go"],
                    "java" => &["*.java"],
                    "c" => &["*.c", "*.h"],
                    "cpp" | "c++" => &["*.cpp", "*.cc", "*.cxx", "*.hpp", "*.hh"],
                    "css" => &["*.css"],
                    "html" => &["*.html"],
                    "swift" => &["*.swift"],
                    "kt" | "kotlin" => &["*.kt"],
                    "scala" => &["*.scala"],
                    "php" => &["*.php"],
                    "sql" => &["*.sql"],
                    "sh" | "bash" | "shell" => &["*.sh"],
                    _ => &[],
                };
                if !globs.is_empty() {
                    for g in globs {
                        cmd.arg("-g").arg(g);
                    }
                    i += 2;
                    continue;
                }
            }
        }
        cmd.arg(arg);
        i += 1;
    }
    // Append the search path only if the replayed args don't already carry it.
    // ripgrep does NOT dedupe path arguments, so passing it twice double-counts
    // every file and inflates the reported savings. When the user passed no path,
    // search_path is "." (clap's default) and is absent from the args, so we add it.
    if !args_slice.iter().any(|a| a == search_path) {
        cmd.arg(search_path);
    }

    let output = match cmd.stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return (0, 0),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut total_matches = 0u64;
    let mut file_count = 0u64;
    for line in stdout.lines() {
        let t = line.trim();
        if t.is_empty() { continue; }
        if let Some(colon) = t.rfind(':') {
            let (p, cpart) = t.split_at(colon);
            if !p.is_empty() {
                if let Ok(cnt) = cpart[1..].trim().parse::<u64>() {
                    if cnt > 0 {
                        total_matches += cnt;
                        file_count += 1;
                    }
                }
            }
        }
    }
    (total_matches, file_count)
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
                // ripgrep includes the trailing line terminator (\n on Unix, \r\n
                // on Windows). Strip both so rendering controls line breaks.
                text: d.get("lines")?.get("text")?.as_str()?
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string(),
            })
        })
        .collect()
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

/// Render matches in ripgrep's output shape for the requested mode. Files are
/// emitted in sorted order: ast-grep's parallel walk is nondeterministic, and a
/// stable order is what makes the output diffable.
fn render_output(matches: &[AgMatch], mode: OutputMode) -> String {
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

// ── ast-grep runner ──────────────────────────────────────────

fn run_ast_grep(sg_pattern: &str, lang: &str, path: &str, inv: &RgInvocation) -> u64 {
    let ag_start = std::time::Instant::now();
    let mut cmd = Command::new("ast-grep");
    cmd.arg("run")
        .arg("-p").arg(sg_pattern)
        .arg("-l").arg(lang);
    // Forward the caller's file filters — ast-grep's --globs takes the same
    // gitignore-style syntax as rg's -g, `!` negation included.
    for g in &inv.globs {
        cmd.arg("--globs").arg(g);
    }
    // Search EVERY path the caller gave, not just the first.
    if inv.paths.is_empty() {
        cmd.arg(path);
    } else {
        for p in &inv.paths {
            cmd.arg(p);
        }
    }
    cmd.arg("--json=stream");

    let output = match cmd.output() {
        Ok(o) => o,
        Err(_) => {
            log_event("ast_grep_error", sg_pattern, "spawn_failed", Some(lang), 0);
            let args: Vec<String> = std::env::args().skip(1).collect();
            exec_real_rg(&args);
        }
    };
    let ag_time_ms = ag_start.elapsed().as_millis() as u64;

    if !output.status.success() {
        // ast-grep exits 1 on "no matches" — that is the normal empty-result case,
        // not a failure, and it writes nothing to stderr. A genuine failure (bad
        // path, unreadable file/stream, internal error) writes to stderr. Only the
        // latter is a real error: log it AND fall back to real rg so the user still
        // gets results instead of a silent empty answer.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            log_event("ast_grep_error", sg_pattern,
                &format!("exit_{}_stderr_{}", output.status, stderr.trim()), Some(lang), 0);
            let args: Vec<String> = std::env::args().skip(1).collect();
            exec_real_rg(&args);
        }
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let matches = parse_ag_matches(&stdout);

    let count = matches.len() as u64;
    let ag_unique_files: HashSet<&str> = matches.iter().map(|m| m.file.as_str()).collect();
    let ag_file_count = ag_unique_files.len() as u64;

    let mode = if inv.count {
        // rg omits the path prefix only when exactly one FILE was named.
        let single_file = inv.paths.len() == 1 && std::path::Path::new(&inv.paths[0]).is_file();
        OutputMode::Count { show_filename: !single_file }
    } else if inv.files_with_matches {
        OutputMode::FilesWithMatches
    } else {
        // Unset → ripgrep's own default: line numbers for a TTY, none when piped.
        OutputMode::Content {
            line_numbers: inv.line_numbers.unwrap_or_else(|| std::io::stdout().is_terminal()),
        }
    };
    print!("{}", render_output(&matches, mode));

    // Capture comparison data (rg vs ast-grep) for the report. Record the RAW
    // user pattern (what rg was counted against), not the translated ast-grep
    // form, so the report's Pattern column matches the numbers beside it.
    //
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

    if matches.is_empty() {
        return 0;
    }
    count
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

    let redirect_rate = if total > 0 { structural as f64 / total as f64 * 100.0 } else { 0.0 };

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

    // By language (structural only)
    let mut by_language = HashMap::new();
    let stmt = conn.prepare(
        "SELECT lang, COUNT(*) FROM events WHERE event='structural' AND lang != '' GROUP BY lang ORDER BY COUNT(*) DESC"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            let l: String = row.get(0)?;
            let c: u64 = row.get(1)?;
            Ok((l, c))
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { by_language.insert(r.0, r.1); }
        }
    }

    // By day (formatted date, not epoch)
    let mut by_day = Vec::new();
    let stmt = conn.prepare(
        "SELECT date(substr(ts, 1, 10), 'unixepoch') as day,
                COUNT(*) as total,
                COUNT(CASE WHEN event='structural' THEN 1 END) as structural
         FROM events GROUP BY day ORDER BY day"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            Ok(DayStats {
                day: row.get(0)?,
                total: row.get(1)?,
                structural: row.get(2)?,
            })
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { by_day.push(r); }
        }
    }

    // Top patterns
    let mut top_patterns = Vec::new();
    let stmt = conn.prepare(
        "SELECT pattern, lang, COUNT(*) as cnt FROM events WHERE event='structural' GROUP BY pattern, lang ORDER BY cnt DESC LIMIT 10"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            Ok(PatternStat {
                pattern: row.get(0)?,
                lang: row.get(1)?,
                count: row.get(2)?,
            })
        }).ok();
        if let Some(rows) = rows {
            for r in rows.flatten() { top_patterns.push(r); }
        }
    }

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
        redirect_rate,
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
        total_intercepted: 0, structural: 0, passthrough: 0, errors: 0,
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
    println!("  Structural redirects: {:>6}  ({:.1}%)", stats.structural, stats.redirect_rate);
    println!("  Passed through (text):{:>6}", stats.passthrough);
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

    // ── dominant_lang: programming beats markup, ties deterministic ──

    fn counts(pairs: &[(&'static str, usize)]) -> std::collections::HashMap<&'static str, usize> {
        pairs.iter().cloned().collect()
    }

    #[test]
    fn programming_language_beats_markup() {
        assert_eq!(dominant_lang(&counts(&[("rust", 1), ("html", 1)])), Some("rust"));
    }

    #[test]
    fn markup_used_only_when_no_programming_language_present() {
        assert_eq!(dominant_lang(&counts(&[("html", 2), ("css", 1)])), Some("html"));
    }

    #[test]
    fn highest_count_wins_among_programming_languages() {
        assert_eq!(dominant_lang(&counts(&[("rust", 3), ("python", 1)])), Some("rust"));
    }

    #[test]
    fn ties_break_alphabetically_for_determinism() {
        assert_eq!(dominant_lang(&counts(&[("go", 2), ("rust", 2)])), Some("go"));
    }

    #[test]
    fn empty_counts_is_none() {
        assert_eq!(dominant_lang(&counts(&[])), None);
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

    #[test]
    fn content_output_is_file_colon_line_colon_text() {
        let m = parse_ag_matches(&ag_json("src/a.ts", 0, "const deviceId = 1;"));
        assert_eq!(
            render_output(&m, OutputMode::Content { line_numbers: true }),
            "src/a.ts:1:const deviceId = 1;\n"
        );
    }

    #[test]
    fn content_output_omits_the_line_number_when_not_asked_for() {
        // Piped `rg PATTERN src` prints `file:content` — no line field.
        let m = parse_ag_matches(&ag_json("src/a.ts", 0, "const deviceId = 1;"));
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
        let stdout = format!(
            "{}\n{}\n{}",
            ag_json("src/a.ts", 0, "a"),
            ag_json("src/a.ts", 4, "b"),
            ag_json("src/b.ts", 2, "c")
        );
        let m = parse_ag_matches(&stdout);
        assert_eq!(
            render_output(&m, OutputMode::Count { show_filename: true }),
            "src/a.ts:2\nsrc/b.ts:1\n"
        );
    }

    #[test]
    fn count_output_omits_the_filename_for_a_single_explicit_file() {
        // `rg -c PATTERN src/a.ts` prints a bare count, no path prefix.
        let m = parse_ag_matches(&format!("{}\n{}", ag_json("src/a.ts", 0, "a"), ag_json("src/a.ts", 4, "b")));
        assert_eq!(render_output(&m, OutputMode::Count { show_filename: false }), "2\n");
    }

    #[test]
    fn files_with_matches_output_is_sorted_and_deduped() {
        let stdout = format!(
            "{}\n{}\n{}",
            ag_json("src/b.ts", 0, "a"),
            ag_json("src/a.ts", 0, "b"),
            ag_json("src/b.ts", 3, "c")
        );
        let m = parse_ag_matches(&stdout);
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

    #[test]
    fn crlf_line_endings_leave_no_stray_carriage_return() {
        // Real `rg --json` emits "text\r\n" for a CRLF-terminated line. Stripping
        // only '\n' baked a '\r' into every match from a Windows-authored file.
        let stream = format!(
            r#"{{"type":"match","data":{{"path":{{"text":"win.ts"}},"lines":{{"text":"const deviceId = 1;\r\n"}},"line_number":1,"absolute_offset":0,"submatches":[]}}}}"#
        );
        let m = parse_rg_json(&stream);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "const deviceId = 1;", "no trailing \\r");
    }
}
