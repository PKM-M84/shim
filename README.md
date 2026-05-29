# shim 🪶

> Drop-in `rg` replacement that silently redirects structural code searches to ast-grep — and tracks how many files, tokens, and dollars it saves you. Saves 50–90% of search tokens. ~2MB binary, bundled SQLite, zero config.

---

## Quick Start

### What does this do?

When your AI coding assistant (Claude Code, Cursor, etc.) searches your code for something like `useState(`, it uses a tool called **ripgrep** (`rg`). ripgrep is fast, but it's dumb — it can't tell the difference between a real `useState()` call in your code and the word "useState" in a comment, a string, or documentation. So it finds 500+ matches when only 60 are real. Your AI opens all 500 files. You pay for all those tokens.

**shim fixes this.** It's a tiny program that pretends to be ripgrep. When your AI calls it, shim quietly checks: "Is this a code pattern or just text?" If it's code, shim sends it to **ast-grep** instead — a smarter tool that actually understands code structure. If it's just text, shim passes it through to real ripgrep. Your AI never knows the difference. You save tokens — and shim records every redirect so you can see the savings (`smart-rg stats` / `smart-rg report`).

### I just want it to work. What do I do?

You don't need anything pre-installed. `install.sh` **checks for and installs what's missing** — ast-grep, ripgrep, and Rust (only if it builds from source). Run it as your **normal user** (not `sudo`); it elevates with `sudo` only to place `rg` in `/usr/local/bin`.

**Option A — fastest (no Rust, no clone): grab the prebuilt binary**

```bash
curl -fsSL https://raw.githubusercontent.com/PKM-M84/shim-/main/install.sh | bash
```

Installs ast-grep + ripgrep (via Homebrew if present), downloads the prebuilt `smart-rg` for your Mac (Apple Silicon or Intel), symlinks `rg`, and configures Claude Code. No Rust required.

**Option B — from source (auto-installs Rust if needed):**

```bash
git clone https://github.com/PKM-M84/shim-.git
cd shim-
./install.sh          # checks deps, installs Rust if missing, builds, installs
```

> Preview without changing anything: `./install.sh --check`. Skip the dependency
> auto-install with `--no-deps`. All flags: `./install.sh --help`.
> Add `--with-grep` only if you also want to intercept `grep` (see warning below).

> **Why `/usr/local/bin`?** It's in macOS's default PATH for every process — terminal, GUI apps, launchd, everything. `~/bin` can break when you restart your AI tool because the new process might not inherit your shell configs. It must come *before* Homebrew's `/opt/homebrew/bin` so your `rg` wins — the installer warns you if it doesn't.

> **Downloaded the binary in a browser** (from the Releases page) instead of via the installer? macOS may quarantine it — clear it with `xattr -dr com.apple.quarantine /usr/local/bin/smart-rg`. The `curl … | bash` flow above is not quarantined.

**Claude Code config (the installer already did this):**

Claude Code ships its **own bundled ripgrep** and ignores your PATH unless you flip one switch. `install.sh` sets it for you — it merges this into `~/.claude/settings.json`:

```json
{
  "env": { "USE_BUILTIN_RIPGREP": "0" }
}
```

If you installed manually (or want to check), add/confirm that block yourself. Skip it with `./install.sh --no-claude-config`. Restart Claude Code so it picks up the new env.

Other tools (Cursor, Codex, Aider, …) shell out to `rg`/`grep` on PATH, so they pick up shim automatically — it works with **any** provider (Anthropic, OpenRouter, DeepSeek, etc.) because it intercepts at the *tool* level, not the model.

**Verify:**

```bash
which rg          # → /usr/local/bin/rg   (NOT /opt/homebrew/bin/rg)
smart-rg stats    # → the shim's stats dashboard (empty until you search — that's fine)
```

That's it. Your AI is now using smarter search, and shim is counting the savings.

### How do I know it's working?

When shim redirects a search, you'll see a cyan message on stderr:

```
🔀 smart-rg → ast-grep (typescript)  pattern: 'useState($$$)'
```

No message = shim passed the search straight to real ripgrep (which is fine — some searches really are just text).

### ⚠️ About intercepting `grep`

`install.sh --with-grep` also points `grep` at the shim. Claude Code calls both `rg` and `grep`, so this widens coverage — **but** the shim speaks ripgrep's flag dialect, not classic `grep`'s, and the symlink shadows the system `grep` for *every* process on your machine. Scripts that rely on real `grep` semantics (`-P`, BRE/ERE, `-o` behavior) can misbehave. Leave it off unless you specifically need it; remove it any time with `sudo rm /usr/local/bin/grep`.

---

## The Story

It started with a question: *"Which file costs the most tokens?"*

May 27th, 2026. 5:56 AM. We were looking at our AI agent's context budget — MEMORY.md at 15KB, read every session, repeated across every sub-agent. But the real bleeding wasn't the bootstrap. It was the cascade: every time the agent needed to find a code pattern, it fired off a text search, opened every matching file, and read them all. False positives in strings, comments, type annotations, test fixtures — every one of them was billable tokens.

We'd heard about ast-grep. A promotional video claimed it could reduce false positives by 122%. Skeptical, we built a benchmarking lab.

### The Benchmarks

We ran 8 structural search patterns against **agentvault-gen2** (1,095 TypeScript files). Text search (ripgrep) vs. ast-grep, measured by **files opened**:

| Pattern | rg files | ast-grep files | Files saved | Tokens saved |
|---|---|---|---|---|
| `async function` | 105 | 9 | **91.4%** | 190,233 |
| `useState(` | 181 | 27 | **85.1%** | 526,237 |
| `try {` | 251 | 51 | **79.7%** | 577,908 |
| `setTimeout(` | 62 | 29 | **53.2%** | 156,095 |
| `await` | 360 | 191 | **46.9%** | 543,897 |
| `fetch(` | 45 | 27 | **40.0%** | 76,515 |
| `process.env` | 43 | 31 | **27.9%** | 44,015 |
| `console.log(` | 34 | 27 | **20.6%** | 49,510 |

**Totals: 689 fewer files opened · 2,164,410 tokens saved · ~119¢ of input cost avoided** (on `deepseek-v4-pro` pricing). shim's built-in report (`smart-rg report`) reproduces this exact breakdown from your own usage.

### The Discovery

We told our agents to prefer ast-grep. We updated the skill files. We added demanding override language to CLAUDE.md: *"ast-grep is the DEFAULT. Grep is the FALLBACK. This is not negotiable."*

Then we checked Claude Code's actual session logs. In a production session on agentvault-gen2: **zero ast-grep calls.** Nine grep calls through Bash. Every single one was a text search that could have been structural.

The conditioning runs deeper than config. The models have "search code → grep" burned into their weights from millions of training examples. System instructions can nudge but can't override.

### The Reverse-Engineering

We needed to know *why* the config wasn't working. So we dug into Claude Code's source:

```
GrepTool.ts → ripgrep.ts → child_process.spawn('rg', args)
```

Claude doesn't call `rg` through the shell. It spawns a **vendored, bundled** ripgrep binary directly via Node's `child_process`. Our PATH-based shell wrappers were invisible to it.

But there was an escape hatch:

```typescript
// ripgrep.ts, line 33
const userWantsSystemRipgrep = isEnvDefinedFalsy(
  process.env.USE_BUILTIN_RIPGREP,
)
```

If `USE_BUILTIN_RIPGREP=0` is set in Claude Code's environment, it bypasses the bundled binary and searches PATH for `rg` instead. A gift.

### The Fix

We built **shim** — a Rust binary that *is* `rg` as far as Claude Code is concerned. Same CLI contract. Same output format. But internally: it classifies every search as structural or textual. Structural patterns go to ast-grep. Everything else passes through to real ripgrep. And every redirect is logged to a local SQLite database so the savings are measurable, not theoretical.

No agent consent required. No model retraining. Just: `USE_BUILTIN_RIPGREP=0` and put shim where `rg` lives.

---

## How It Works

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Claude Code  │────→│     shim     │────→│   ast-grep   │  (structural)
│  (or any tool)│     │  classifies  │     │              │
│  calls `rg`   │     │  pattern     │────→│   real rg    │  (text search)
└──────────────┘     └──────┬───────┘     └──────────────┘
                            │ logs every call + rg-vs-ast comparison
                            ▼
                     ~/.smart-rg/stats.db  →  smart-rg stats / report
```

1. **Classify** — Strip rg regex escapes, analyze the pattern. Function call? Keyword? Structural construct? Or complex regex that needs real ripgrep?
2. **Translate** — `console.log(` → `console.log($$$)`; `useState` → `useState`; `async function` → `async function $$$($$$) { $$$ }`.
3. **Execute** — Run ast-grep with `--json=stream`, parse the AST match data.
4. **Reformat** — Rewrite ast-grep's JSON to `file:line:content` — the exact format rg produces and Claude Code's parser expects.
5. **Measure** — Record the search (and a head-to-head rg-vs-ast comparison) to SQLite for the savings report.
6. **Fall back** — If anything goes wrong (ast-grep fails, pattern can't be translated, unrecognized flags), shim silently falls through to real ripgrep. The agent never knows.

---

## Stats & Reports

shim logs every search to a local SQLite database (`~/.smart-rg/stats.db`, created automatically) and turns it into savings numbers.

```bash
smart-rg stats                         # terminal dashboard
smart-rg stats --json                  # machine-readable
smart-rg report -o report.html --open  # self-contained HTML report with charts
```

The HTML report shows the **rg vs ast-grep comparison** per pattern — files matched, estimated tokens, and cost saved — the same shape as the benchmark table above. Comparison rows can carry **real** token/cost figures; live captures fall back to a `matches × 15 tokens @ $2/M` estimate.

Two optional env vars:

| Var | Default | Purpose |
|---|---|---|
| `SMART_RG_AGENT` | `unknown` | Tags each logged search with an agent name so `stats` can break savings down per agent (`export SMART_RG_AGENT=claude-code`). |
| `SMART_RG_HOME` | `~/.smart-rg` | Where `stats.db` lives. Point elsewhere to test without touching your real stats. |

> **Privacy:** the database is local only. shim never phones home — no analytics, no telemetry.

---

## Installation (details)

See [Quick Start](#quick-start) for the simple version. `install.sh` installs these for you (unless `--no-deps`); listed here for reference:

- [ast-grep](https://ast-grep.github.io/) (`brew install ast-grep` or `npm install -g @ast-grep/cli`) — **must be on PATH**
- ripgrep (`brew install ripgrep`) — shim falls back to it for text searches
- [Rust](https://rustup.rs/) — only when building from source (Option B); not needed for the prebuilt binary

### Claude Code configuration

`install.sh` automatically merges `USE_BUILTIN_RIPGREP=0` into `~/.claude/settings.json` (so Claude uses your PATH `rg` instead of its bundled one). If you installed manually, add it yourself:

```json
{ "env": { "USE_BUILTIN_RIPGREP": "0" } }
```

For broader coverage you *can* also intercept `grep` (`./install.sh --with-grep`), but read the [grep warning](#️-about-intercepting-grep) first.

### Cursor, Codex, Copilot CLI, Aider, …

Any tool that shells out to `rg` works automatically — just make sure `/usr/local/bin` (where shim lives) comes before the system binaries in PATH. It intercepts at the tool level, not the model, so it works with **any** provider.

### Docker & containerized environments

Tools that run inside containers/sandboxes have their own PATH and won't see your host's shim. Install it inside:

```dockerfile
COPY smart-rg /usr/local/bin/
RUN ln -sf /usr/local/bin/smart-rg /usr/local/bin/rg
```

Same idea for devcontainers, CI runners, or any sandbox — get the binary onto the container's PATH.

### Troubleshooting

| Symptom | Fix |
|---|---|
| `which rg` shows `/opt/homebrew/bin/rg` | PATH order: `/usr/local/bin` must come *before* Homebrew. Interception is silently off until fixed. |
| Searches work but `smart-rg stats` is empty | You're hitting real `rg` (PATH issue above), **or** Claude Code is using its bundled rg — set `USE_BUILTIN_RIPGREP=0`. |
| Structural searches all fall through to text | `ast-grep` isn't on PATH. `which ast-grep`; `brew install ast-grep`. |
| Worked, then stopped after restarting the AI tool | You installed to `~/bin`. Use `/usr/local/bin` (universal PATH for all processes). |
| Ordinary `grep` started behaving weirdly | You used `--with-grep`. Remove it: `sudo rm /usr/local/bin/grep`. |
| **Roll back everything** | `sudo rm /usr/local/bin/rg /usr/local/bin/grep /usr/local/bin/smart-rg` and drop the `USE_BUILTIN_RIPGREP` line. Back to stock. |

---

## Usage

```bash
# Drop-in rg-compatible
smart-rg 'useState(' --type ts ./src
smart-rg -n -i 'auth'  --type ts ./src
smart-rg -l 'describe(' --type ts ./src
smart-rg -c 'console.log(' --type ts ./src

# Redirected to ast-grep:
#   Function calls:  console.log(     → console.log($$$)
#   Method refs:     process.env      → process.env
#   Keywords:        await            → await
#   Declarations:    async function   → async function $$$($$$) { $$$ }

# Passed through to real rg:
#   Complex regex:   import.*from
#   Text search:     TODO, FIXME
#   Non-code files:  --type md, --type json
```

---

## Architecture

```
src/main.rs
├── Cli (clap)           — rg-compatible flag parser
├── classify()           — structural vs. text decision
├── translate_pattern()  — rg regex → ast-grep pattern
├── map_lang()           — rg --type → ast-grep language
├── run_ast_grep()       — execute ast-grep, parse JSON, reformat, capture comparison
├── run_rg_count()       — replay rg --count for the rg-vs-ast savings baseline
├── log_event() / log_comparison()  — SQLite logging (events + comparisons)
├── compute_stats() / generate_report()  — stats + self-contained HTML report
└── exec_real_rg()       — fallback to real ripgrep (absolute path; avoids symlink loop)
```

**Dependencies:** clap (CLI), rusqlite (bundled SQLite — no system SQLite needed), serde + serde_json (ast-grep output parsing). **Binary:** ~2MB release; the HTML report template is compiled in via `include_str!`.

**Database** (`~/.smart-rg/stats.db`):
- `events` — every intercepted search: `agent, event, pattern, reason, lang, matches, ts`
- `comparisons` — rg-vs-ast savings, incl. real token/cost columns: `pattern, lang, ag_matches, ag_files, rg_results, rg_files, files_saved, estimated_tokens_saved, estimated_cost_saved_cents, text_tokens, ast_tokens, text_cost_cents, ast_cost_cents, ts`

---

## Tests

The installer's Claude-settings merge has a smoke test covering the jq + python3
engines and the fresh / existing / idempotent / malformed cases (no root needed):

```bash
bash tests/install_test.sh
```

---

## Safety

- **Zero data leaves your machine.** No phone-home, no analytics, local SQLite only.
- **Graceful fallback.** Any error, unrecognized pattern, or unhandled flag → falls through to real ripgrep. The search always completes.
- **Opt-in.** Active only when `USE_BUILTIN_RIPGREP=0` (Claude Code) or when placed first in PATH. Remove either and you're back to stock ripgrep.
- **Transparent.** The `🔀 smart-rg → ast-grep` line prints to stderr so you can see redirects. Pass-throughs are silent.

---

## Performance

Benchmarked on agentvault-gen2 (1,095 TS files, M2 Mac mini):

| Operation | real rg | shim (redirected) |
|---|---|---|
| `useState(` search | ~30ms | ~70ms |
| `console.log(` search | ~30ms | ~70ms |
| `import.*from` (passthrough) | ~25ms | ~25ms |

Redirection adds ~40ms (ast-grep's JSON output is larger). For the token savings, that's negligible — every false-positive file the agent *doesn't* open saves thousands of tokens.

---

## Roadmap

- [ ] Flag translation: `-A`, `-B`, `-C`, `--glob` in ast-grep mode
- [ ] Advanced classification: decorators, type annotations, JSX
- [ ] Output streaming instead of buffering ast-grep results
- [ ] Language auto-detection from file extensions when `--type` is omitted
- [ ] `smart-rg install` / `doctor` subcommands (self-installer, replaces the manual symlink dance)
- [ ] Homebrew formula

---

## Contributing

Found a pattern that should redirect but doesn't? Check `smart-rg stats` for recent pass-throughs, then open an issue/PR with the pattern. Welcome.

## License

MIT

---

*Built by [Wren](https://x.com/WrenLogic) & Chris. Because your AI agent burns tokens on bad search, and the platforms profit from it.*
