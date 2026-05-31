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
use std::collections::{HashMap, HashSet};
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
#[command(name = "smart-rg", version = "0.3.2")]
#[command(disable_help_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// The search pattern (regex)
    pattern: Option<String>,

    /// File or directory to search
    #[arg(default_value = ".")]
    path: String,

    #[arg(long = "type", short = 't')]
    file_type: Option<String>,

    #[arg(short = 'l', long = "files-with-matches")]
    files_with_matches: bool,

    #[arg(short = 'n', long = "line-number")]
    line_number: bool,

    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    #[arg(short = 'B', allow_hyphen_values = true)]
    before_context: Option<String>,

    #[arg(short = 'A', allow_hyphen_values = true)]
    after_context: Option<String>,

    #[arg(short = 'C', allow_hyphen_values = true)]
    context: Option<String>,

    #[arg(long = "glob", short = 'g')]
    glob: Option<String>,

    #[arg(short = 'v', long = "invert-match")]
    invert_match: bool,

    #[arg(short = 'c', long = "count")]
    count: bool,

    #[arg(short = 'r', long = "recursive")]
    recursive: bool,

    #[arg(long = "hidden")]
    hidden: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    extra: Vec<String>,
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

// ── Main ─────────────────────────────────────────────────────

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

    // Passthrough modes: no args, --help, -h
    if args.len() <= 1 || args.contains(&"--help".to_string()) || args.contains(&"-h".to_string()) {
        exec_real_rg(&args[1..]);
    }

    let cli = match Cli::try_parse_from(args.iter()) {
        Ok(c) => c,
        Err(_) => {
            let maybe_pattern = args.iter().skip(1)
                .find(|a| !a.starts_with('-') && !a.starts_with("--"));
            if let Some(pat) = maybe_pattern {
                log_event("parse_error", pat, "clap_failed", None, 0);
            }
            exec_real_rg(&args[1..]);
        }
    };

    let pattern = match &cli.pattern {
        Some(p) => p.clone(),
        None => {
            exec_real_rg(&args[1..]);
        }
    };

    let lang = map_lang(&cli.file_type);
    let is_structural = classify(&pattern);

    if !is_structural || lang.is_none() {
        let reason = if !is_structural { "not_structural" } else { "no_language" };
        log_event("passthrough", &pattern, reason, lang, 0);
        exec_real_rg(&args[1..]);
    }

    let lang = lang.unwrap();
    let sg_pattern = translate_pattern(&pattern);

    eprintln!("\x1b[36m🔀 smart-rg → ast-grep ({})  pattern: '{}'\x1b[0m", lang, sg_pattern);

    let match_count = run_ast_grep(&sg_pattern, lang, &cli.path, &cli);

    // Log the successful redirect
    log_event("structural", &sg_pattern, "redirected", Some(lang), match_count);

    if match_count == 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::process::exit(1);
    }
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

// ── Classification ───────────────────────────────────────────

fn classify(pattern: &str) -> bool {
    // Regex patterns are never structural — pass through to rg
    if pattern.contains('\\') {
        return false;
    }

    let raw = pattern.trim();

    if raw.is_empty() || raw.len() <= 1 {
        return false;
    }

    // Structural indicators
    let has_mixed_case = raw.chars().any(|c| c.is_uppercase()) && raw.chars().any(|c| c.is_lowercase());
    let has_snake = raw.contains('_');
    let has_structural = raw.contains('.') || raw.contains("::")
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

    // Function-call shorthand: "foo(" or "obj.method("
    if raw.contains('(') && !raw.contains('|') && !raw.contains('[') {
        return raw.ends_with('(') || raw.contains(".(");
    }

    // Accept identifier-like forms OR any pattern with explicit structural operators
    let is_id_like = raw.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ' ' || c == '.');
    if is_id_like || has_structural {
        return has_mixed_case || has_snake || has_structural;
    }

    false
}

// ── Pattern translation ──────────────────────────────────────

fn translate_pattern(pattern: &str) -> String {
    let raw: String = pattern.chars().filter(|&c| c != '\\').collect();
    let raw = raw.trim();

    if let Some(stripped) = raw.strip_suffix('(') {
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
    // -F (literal): the intercepted pattern is often a structural form like `foo(`
    // that is an INVALID regex for ripgrep. Without -F the baseline silently errors
    // to 0 results and the reported savings collapse for exactly the paren-style
    // patterns the shim is built to redirect.
    cmd.arg("--count").arg("-F");
    let mut i = 0;
    let args_slice = original_args;
    while i < args_slice.len() {
        let arg = &args_slice[i];
        // Skip output-mode flags that conflict with --count
        if matches!(arg.as_str(), "-c" | "--count" | "-l" | "--files-with-matches"
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
    let estimated_cost_saved_cents = text_cost_cents - ast_cost_cents;

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

// ── ast-grep runner ──────────────────────────────────────────

fn run_ast_grep(sg_pattern: &str, lang: &str, path: &str, cli: &Cli) -> u64 {
    let ag_start = std::time::Instant::now();
    let mut cmd = Command::new("ast-grep");
    cmd.arg("run")
        .arg("-p").arg(sg_pattern)
        .arg("-l").arg(lang)
        .arg(path)
        .arg("--json=stream");

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
        let stderr = String::from_utf8_lossy(&output.stderr);
        log_event("ast_grep_error", sg_pattern,
            &format!("exit_{}_stderr_{}", output.status, stderr.trim()), Some(lang), 0);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let matches: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() { return None; }
            serde_json::from_str(trimmed).ok()
        })
        .collect();

    let count = matches.len() as u64;
    let mut ag_unique_files = HashSet::new();
    for m in &matches {
        if let Some(f) = m.get("file").and_then(|f| f.as_str()) {
            ag_unique_files.insert(f.to_string());
        }
    }
    let ag_file_count = ag_unique_files.len() as u64;

    if cli.count {
        println!("{}", count);
    } else if cli.files_with_matches {
        let mut files: Vec<&str> = matches.iter()
            .filter_map(|m| m.get("file").and_then(|f| f.as_str()))
            .collect();
        files.sort();
        files.dedup();
        for f in files { println!("{}", f); }
    } else {
        for m in &matches {
            let file = m.get("file").and_then(|f| f.as_str()).unwrap_or("");
            let start_line = m.get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(|l| l.as_u64())
                .unwrap_or(0);
            let content = m.get("lines")
                .and_then(|l| l.as_str())
                .or_else(|| m.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            println!("{}:{}:{}", file, start_line, content);
        }
    }

    // Capture comparison data (rg vs ast-grep) for ROI report. Record the RAW
    // user pattern (what rg was counted against), not the translated ast-grep
    // form, so the report's Pattern column matches the numbers beside it.
    if count > 0 {
        let raw_pattern = cli.pattern.as_deref().unwrap_or(sg_pattern);
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
    total_files_saved: u64,
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

fn compute_stats() -> StatsReport {
    let conn = match open_db() {
        Some(c) => c,
        None => return empty_stats(),
    };

    // Lazy retention: prune old events when the (infrequent, human-run) stats/report
    // is generated, instead of on every search.
    let _ = prune_old_events(&conn, 30);

    let total: u64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap_or(0);
    if total == 0 {
        return empty_stats();
    }

    let structural: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event='structural'", [], |r| r.get(0)
    ).unwrap_or(0);

    let passthrough: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event='passthrough'", [], |r| r.get(0)
    ).unwrap_or(0);

    let errors: u64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event LIKE '%error%' OR event='untranslatable'",
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

    // Comparison data (rg vs ag savings)
    let mut comparisons = Vec::new();
    let mut total_files_saved = 0u64;
    let mut total_tokens_saved = 0u64;
    let mut total_cost_saved = 0.0f64;
    let stmt = conn.prepare(
        "SELECT pattern, lang, ag_matches, ag_files, ag_time_ms, rg_results, rg_files, rg_time_ms, files_saved, estimated_tokens_saved, estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents, ast_cost_cents
         FROM comparisons ORDER BY id DESC LIMIT 50"
    ).ok();
    if let Some(mut s) = stmt {
        let rows = s.query_map([], |row| {
            let fs: u64 = row.get(8)?;
            let est_toks: u64 = row.get(9)?;
            let est_cost: f64 = row.get(10)?;
            let text_tokens: u64 = row.get(11)?;
            let ast_tokens: u64 = row.get(12)?;
            let text_cost: f64 = row.get(13)?;
            let ast_cost: f64 = row.get(14)?;
            // Mirror the report front-end's precedence (estimated_* || text − ast)
            // so a seeded benchmark row whose estimate columns are 0 still feeds the
            // headline KPIs — otherwise the totals silently disagree with the table.
            let toks = if est_toks > 0 { est_toks } else { text_tokens.saturating_sub(ast_tokens) };
            let cost = if est_cost != 0.0 { est_cost } else { text_cost - ast_cost };
            total_files_saved += fs;
            total_tokens_saved += toks;
            total_cost_saved += cost;
            Ok(ComparisonStat {
                pattern: row.get(0)?,
                lang: row.get(1)?,
                ag_matches: row.get(2)?,
                ag_files: row.get(3)?,
                ag_time_ms: row.get(4)?,
                rg_results: row.get(5)?,
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
        total_files_saved,
        total_tokens_saved_estimate: total_tokens_saved,
        total_cost_saved_cents: total_cost_saved,
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
        total_files_saved: 0, total_tokens_saved_estimate: 0, total_cost_saved_cents: 0.0,
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
        let mut total_files_saved = 0u64;
        let mut total_tokens_saved = 0u64;
        for c in &stats.comparisons {
            println!("  {:<25} {:>10} {:>10} {:>10} {:>10}",
                c.pattern, c.ag_files, c.rg_files, c.files_saved, c.estimated_tokens_saved);
            total_files_saved += c.files_saved;
            total_tokens_saved += c.estimated_tokens_saved;
        }
        println!();
        println!("  Total files saved:  {:>10}", total_files_saved);
        println!("  Total tokens saved: {:>10}", total_tokens_saved);
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
    let html = REPORT_TEMPLATE.replace("__SHIM_DATA__", &data_json);

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
