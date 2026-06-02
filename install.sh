#!/bin/bash
# smart-rg installer — makes `rg` resolve to the smart-rg shim ahead of the real
# ripgrep for every terminal-launched agent shell (Claude Code, aider, OpenRouter
# / DeepSeek CLIs, …).
#
# Model (v0.3+): the shim lives in a DEDICATED dir ~/.smart-rg/bin/rg, and that
# dir is forced to the front of PATH via a drop-in (~/.smart-rg/env.sh) sourced
# from a marked block in every shell startup file that applies. The real ripgrep
# is symlinked to ~/.smart-rg/bin/rg2 so the shim can forward without ever
# resolving back to itself (which would fork-bomb on Linux — see real_rg_path in
# src/main.rs). USE_BUILTIN_RIPGREP=0 is set in ~/.claude/settings.json only.
#
#   ./install.sh                      install / upgrade (idempotent)
#   ./install.sh --uninstall          remove block + bin dir + drop-in (keeps stats.db)
#   ./install.sh --uninstall --purge  also delete ~/.smart-rg/stats.db
#   ./install.sh --check              report dependencies, make no changes
#   ./install.sh --no-deps            skip auto-installing ast-grep / ripgrep / Rust
#   ./install.sh --no-claude-config   leave ~/.claude/settings.json untouched
#
# Targets bash/zsh only: macOS+zsh, Linux+bash, Linux+zsh. Not Windows/fish.

set -e

REPO="PKM-M84/shim"

# ── helpers ───────────────────────────────────────────────────────────────────
# Run user-space tools (brew, rustup, cargo) as the REAL user even under sudo.
REAL_USER="${SUDO_USER:-$(whoami)}"
REAL_HOME="${SMARTRG_HOME_OVERRIDE:-$(eval echo "~$REAL_USER")}"   # SMARTRG_HOME_OVERRIDE: tests only
as_user()   { if [ "$EUID" -eq 0 ] && [ "$REAL_USER" != "root" ]; then sudo -u "$REAL_USER" -H "$@"; else "$@"; fi; }
need_root() { if [ "$EUID" -eq 0 ]; then "$@"; else sudo "$@"; fi; }
uhave()     { as_user bash -lc "command -v '$1'" >/dev/null 2>&1; }   # is <cmd> on the real user's PATH?
ulc()       { as_user bash -lc "$1"; }                                # run a command line as the real user

SRG_HOME="$REAL_HOME/.smart-rg"
SRG_BIN="$SRG_HOME/bin"
ENV_SH="$SRG_HOME/env.sh"

MARKER_BEGIN="# >>> smart-rg >>>"
MARKER_END="# <<< smart-rg <<<"

# ── target shell startup files ────────────────────────────────────────────────
# We install into ALL that apply, for BOTH shells (we do not trust $SHELL alone).
# zsh reads .zshenv even in non-interactive agent shells; .zshrc re-asserts after
# Homebrew in interactive shells. For bash we touch .bashrc plus exactly one login
# file (.bash_profile shadows .profile, so never write both).
target_files() {
    printf '%s\n' "$REAL_HOME/.zshenv"
    printf '%s\n' "$REAL_HOME/.zshrc"
    printf '%s\n' "$REAL_HOME/.bashrc"
    if [ -f "$REAL_HOME/.bash_profile" ]; then
        printf '%s\n' "$REAL_HOME/.bash_profile"
    else
        printf '%s\n' "$REAL_HOME/.profile"
    fi
}

# ── PATH drop-in ──────────────────────────────────────────────────────────────
write_env_sh() {
    mkdir -p "$SRG_HOME"
    cat > "$ENV_SH" <<'ENVEOF'
# Managed by smart-rg. Prepends ~/.smart-rg/bin to PATH (position 1), deduping
# any existing occurrences (including the adjacent-duplicate edge case).
# bash/zsh only.
_srg="$HOME/.smart-rg/bin"
if [ -d "$_srg" ]; then
  while case ":$PATH:" in *":$_srg:"*) true;; *) false;; esac; do
    PATH=":$PATH:"; PATH="${PATH//:$_srg:/:}"; PATH="${PATH#:}"; PATH="${PATH%:}"
  done
  PATH="$_srg:$PATH"; export PATH
fi
unset _srg
ENVEOF
    echo "✓ Wrote PATH drop-in → $ENV_SH"
}

# ── marked-block management (portable awk + mktemp; never sed -i) ──────────────
# Remove our marked block AND any legacy single-line installs from a file. The
# legacy installs match 'added by smart-rg installer' / 'smart-rg: prefer' and
# skip the marker line plus the export line right after it (old format).
strip_block() {
    local file="$1"
    [ -f "$file" ] || return 0
    local tmp tmp2
    tmp="$(mktemp)"; tmp2="$(mktemp)"
    awk -v b="$MARKER_BEGIN" -v e="$MARKER_END" '
        /added by smart-rg installer/ || /smart-rg: prefer/ { legacy=1; next }
        legacy { legacy=0; next }
        $0 == b { inblk=1; next }
        $0 == e { inblk=0; next }
        inblk { next }
        { print }
    ' "$file" > "$tmp"
    # Collapse any trailing run of blank lines we may have left.
    awk '{lines[NR]=$0} END{
        last=NR; while (last>0 && lines[last]=="") last--;
        for (i=1;i<=last;i++) print lines[i]
    }' "$tmp" > "$tmp2"
    cat "$tmp2" > "$file"
    rm -f "$tmp" "$tmp2"
    return 0
}

# Append our marked block to a file (creating it), only if absent. Idempotent.
add_block() {
    local file="$1"
    touch "$file"
    if grep -qF "$MARKER_BEGIN" "$file" 2>/dev/null; then
        echo "  ↩︎  already present in $file"
        return 0
    fi
    {
        printf '\n%s\n' "$MARKER_BEGIN"
        printf '%s\n' '[ -f "$HOME/.smart-rg/env.sh" ] && . "$HOME/.smart-rg/env.sh"'
        printf '%s\n' "$MARKER_END"
    } >> "$file"
    echo "✓ Updated $file"
    return 0
}

# ── is the given rg path actually one of OUR shims? ──────────────────────────
# True for a symlink whose target mentions smart-rg, the dedicated shim path, or
# a binary that embeds the shim's "smart-rg:" signature. Real-rg resolution uses
# this to skip the shim by CONTENT, not just by path string — so a stale shim
# left at /opt/homebrew/bin/rg can never be picked as "real rg" and fork-bomb.
is_smart_rg_shim() {
    local p="$1"
    [ "$p" = "$SRG_BIN/rg" ] && return 0
    case "$(readlink "$p" 2>/dev/null || true)" in *smart-rg*) return 0 ;; esac
    grep -aq "smart-rg:" "$p" 2>/dev/null && return 0
    return 1
}

# ── real ripgrep resolution (STABLE path; never our shim) ─────────────────────
resolve_real_rg() {
    local c
    for c in /opt/homebrew/bin/rg /usr/local/bin/rg /usr/bin/rg; do
        if [ -x "$c" ] && ! is_smart_rg_shim "$c"; then printf '%s\n' "$c"; return 0; fi
    done
    local IFS=:
    for c in $PATH; do
        case "$c" in "$SRG_BIN") continue ;; esac
        if [ -x "$c/rg" ] && ! is_smart_rg_shim "$c/rg"; then printf '%s\n' "$c/rg"; return 0; fi
    done
    return 1
}

# ── migrate away old shims that would shadow the new dedicated dir ─────────────
# Pre-0.3 installs put a `rg`/`grep` symlink or a `smart-rg` binary in
# /usr/local/bin, and the old PATH-fix could drop a `rg` symlink into ANY
# user-writable dir that was ahead of Homebrew on PATH (commonly ~/.local/bin or
# ~/bin). So we don't probe a fixed list — we scan every PATH dir plus those
# well-known legacy spots and remove anything OUTSIDE the dedicated bin that is
# unmistakably OURS (a symlink → smart-rg, or a binary with the "smart-rg:"
# signature). Real rg/grep are never flagged, so upgrades leave no orphans behind.
migrate_old_shim() {
    local d f p cands seen
    IFS=: read -ra cands <<< "$PATH"
    cands+=( /usr/local/bin "$REAL_HOME/.local/bin" "$REAL_HOME/bin" )
    seen=" "
    for d in "${cands[@]}"; do
        [ -n "$d" ] || continue
        case "$seen" in *" $d "*) continue ;; esac   # dedupe PATH entries
        seen="$seen$d "
        [ "$d" = "$SRG_BIN" ] && continue             # never touch the new dir
        for f in rg grep smart-rg; do
            p="$d/$f"
            { [ -e "$p" ] || [ -L "$p" ]; } || continue   # exists or dangling symlink
            if is_smart_rg_shim "$p"; then
                need_root rm -f "$p" 2>/dev/null || rm -f "$p"
                echo "  removed old shim/command $p"
            fi
        done
    done
    # legacy manifest is obsolete in the new model
    rm -f "$SRG_HOME/manifest" 2>/dev/null || true
    return 0
}

# ── Claude Code settings merge (testable, no root, no install needed) ─────────
# Sets env.USE_BUILTIN_RIPGREP="0" in the given settings.json while preserving
# everything else. Honors SMARTRG_JSON_ENGINE=jq|python3|auto (default: auto)
# so the smoke test can exercise each code path. Returns non-zero if no JSON
# tool is available. USE_BUILTIN_RIPGREP lives ONLY here — never in the shell env.
merge_claude_setting() {
    local settings="$1"
    local engine="${SMARTRG_JSON_ENGINE:-auto}"
    mkdir -p "$(dirname "$settings")"

    if [ ! -s "$settings" ]; then
        printf '{\n  "env": { "USE_BUILTIN_RIPGREP": "0" }\n}\n' > "$settings"
        echo "✓ Created $settings (USE_BUILTIN_RIPGREP=0)"
        return 0
    fi

    if { [ "$engine" = "jq" ] || [ "$engine" = "auto" ]; } && command -v jq >/dev/null 2>&1; then
        local tmp; tmp="$(mktemp)"
        if jq '.env = (((.env // {}) | if type == "object" then . else {} end) + {"USE_BUILTIN_RIPGREP":"0"})' \
               "$settings" > "$tmp" 2>/dev/null && [ -s "$tmp" ]; then
            mv "$tmp" "$settings"
            echo "✓ Set USE_BUILTIN_RIPGREP=0 in $settings (jq)"
            return 0
        fi
        rm -f "$tmp"
    fi

    if { [ "$engine" = "python3" ] || [ "$engine" = "auto" ]; } && command -v python3 >/dev/null 2>&1; then
        if python3 - "$settings" <<'PY'
import json, sys
p = sys.argv[1]
try:
    d = json.load(open(p))
    if not isinstance(d, dict): d = {}
except Exception:
    d = {}
if not isinstance(d.get("env"), dict):
    d["env"] = {}
d["env"]["USE_BUILTIN_RIPGREP"] = "0"
with open(p, "w") as f:
    json.dump(d, f, indent=2); f.write("\n")
PY
        then
            echo "✓ Set USE_BUILTIN_RIPGREP=0 in $settings (python3)"
            return 0
        fi
    fi

    echo "⚠️  Could not edit $settings (need jq or python3). Add manually:"
    echo '      "env": { "USE_BUILTIN_RIPGREP": "0" }'
    return 1
}

# Internal/convenience entrypoint: run ONLY the settings merge (no root, no
# install). Used by tests; also handy to re-apply the Claude config standalone.
#   ./install.sh --merge-claude-config [path]   (default: ~/.claude/settings.json)
if [ "${1:-}" = "--merge-claude-config" ]; then
    merge_claude_setting "${2:-$HOME/.claude/settings.json}"
    exit $?
fi

# ── dependency detection / reporting ──────────────────────────────────────────
brew_path() {
    local b; b="$(ulc 'command -v brew' 2>/dev/null || true)"
    if [ -n "$b" ]; then echo "$b";
    elif [ -x /opt/homebrew/bin/brew ]; then echo /opt/homebrew/bin/brew;
    elif [ -x /usr/local/bin/brew ]; then echo /usr/local/bin/brew; fi
}
report_deps() {
    echo "  ripgrep (rg):  $(uhave rg        && echo present || echo MISSING)"
    echo "  ast-grep:      $(uhave ast-grep  && echo present || echo MISSING)"
    echo "  cargo (Rust):  $(uhave cargo     && echo present || echo 'missing (only needed to build from source)')"
    echo "  Homebrew:      $([ -n "$(brew_path)" ] && echo present || echo 'missing')"
    echo "  arch:          $(uname -m)"
}

if [ "${1:-}" = "--check" ]; then
    echo "🪶 smart-rg — dependency check (no changes made)"
    report_deps
    exit 0
fi

# ── self-verify ───────────────────────────────────────────────────────────────
# Probe a fresh shell with a SANITIZED PATH, so we measure what the startup files
# do (not what the installer's own PATH happened to contain).
# Extract ONLY the resolved rg path (a line ending in /rg). Startup files often
# print their own noise (oh-my-zsh, powerlevel10k, MOTD, session-restore) before
# the `-c` command runs; without this filter that noise contaminates the result
# and the verify falsely FAILs even when rg resolves correctly.
probe()   { PATH="/usr/bin:/bin" "$@" -c 'command -v rg' 2>/dev/null | grep -E '/rg$' | tail -n1 || true; }
in_shim() { case "$1" in "$SRG_BIN"/*) return 0 ;; *) return 1 ;; esac; }

self_verify() {
    local ok=1
    echo "🔎 Verifying rg resolution in fresh shells…"
    if command -v zsh >/dev/null 2>&1; then
        # zsh reads .zshenv for BOTH interactive and non-interactive shells.
        local zi zn
        zi="$(HOME="$REAL_HOME" probe zsh -i)"; zn="$(HOME="$REAL_HOME" probe zsh)"
        echo "   zsh  interactive:      ${zi:-<none>}"
        echo "   zsh  non-interactive:  ${zn:-<none>}"
        in_shim "$zi" || ok=0
        in_shim "$zn" || ok=0
    fi
    if command -v bash >/dev/null 2>&1; then
        # bash reads .bashrc when interactive and the login file when login; a
        # plain `bash -c` reads NEITHER (only $BASH_ENV), so it is info-only.
        local bi bl bn
        bi="$(HOME="$REAL_HOME" probe bash -i)"; bl="$(HOME="$REAL_HOME" probe bash -l)"; bn="$(HOME="$REAL_HOME" probe bash)"
        echo "   bash interactive:      ${bi:-<none>}"
        echo "   bash login:            ${bl:-<none>}"
        echo "   bash non-interactive:  ${bn:-<none>}  (info only; bash -c reads no rc)"
        in_shim "$bi" || ok=0
        in_shim "$bl" || ok=0
    fi
    if [ "$ok" -eq 1 ]; then
        echo "✅ self-verify PASS (rg resolves into $SRG_BIN)"
    else
        echo "❌ self-verify FAIL"
        echo "   Check that the marker block is present and sourced in your startup"
        echo "   files, and that no later line re-prepends another rg dir."
    fi
    return 0
}

# ── uninstall ─────────────────────────────────────────────────────────────────
do_uninstall() {
    echo "🧹 Uninstalling smart-rg..."
    local f
    while IFS= read -r f; do
        if [ -f "$f" ] && grep -qE 'smart-rg|added by smart-rg installer' "$f" 2>/dev/null; then
            strip_block "$f"
            echo "  cleaned smart-rg lines from $f"
        fi
    done < <(target_files)

    # New-model artifacts
    rm -f "$SRG_BIN/rg" "$SRG_BIN/rg2" "$SRG_BIN/smart-rg"
    rmdir "$SRG_BIN" 2>/dev/null || true
    rm -f "$ENV_SH"

    # Old-model artifacts (so upgraders don't leak stale state)
    migrate_old_shim

    if [ "${PURGE:-0}" -eq 1 ]; then
        # Remove the DB and its WAL sidecars (explicit names, so user backups
        # like stats.db.bak-* are NOT swept up) — else -wal/-shm orphan the dir.
        rm -f "$SRG_HOME/stats.db" "$SRG_HOME/stats.db-wal" "$SRG_HOME/stats.db-shm"
        rmdir "$SRG_HOME" 2>/dev/null || true
        echo "  removed ~/.smart-rg/stats.db (purged)"
    else
        rmdir "$SRG_HOME" 2>/dev/null || true   # only succeeds if already empty
        echo "  kept ~/.smart-rg/stats.db (re-run with --purge to remove)"
    fi
    echo "✅ Uninstalled. (Claude's USE_BUILTIN_RIPGREP=0 left in ~/.claude/settings.json — harmless.)"
    return 0
}

# ── flag parsing ──────────────────────────────────────────────────────────────
echo "🪶 smart-rg installer"
echo ""

CONFIGURE_CLAUDE=1
INSTALL_DEPS=1
UNINSTALL=0
PURGE=0
for arg in "$@"; do
  case "$arg" in
    --no-claude-config) CONFIGURE_CLAUDE=0 ;;
    --no-deps) INSTALL_DEPS=0 ;;
    --uninstall) UNINSTALL=1 ;;
    --purge) PURGE=1 ;;
    -h|--help)
      echo "Usage: ./install.sh [flags]"
      echo "  --no-claude-config      leave ~/.claude/settings.json untouched"
      echo "  --no-deps               skip auto-installing ast-grep / ripgrep / Rust"
      echo "  --uninstall             remove the shim, PATH drop-in, and shell blocks"
      echo "  --purge                 with --uninstall, also delete ~/.smart-rg/stats.db"
      echo "  --check                 report what's installed and exit (no changes)"
      echo "  --merge-claude-config [path]   only set USE_BUILTIN_RIPGREP=0 and exit (no root)"
      exit 0 ;;
  esac
done

if [ "$UNINSTALL" -eq 1 ]; then
    do_uninstall
    exit 0
fi

# ── install runtime dependencies (ast-grep, ripgrep) ──────────────────────────
if [ "$INSTALL_DEPS" -eq 1 ]; then
    BREW="$(brew_path)"
    if ! uhave ast-grep; then
        echo "• ast-grep missing — installing..."
        if [ -n "$BREW" ]; then as_user "$BREW" install ast-grep || echo "  ⚠️  brew install ast-grep failed"
        elif uhave npm; then ulc 'npm install -g @ast-grep/cli' || echo "  ⚠️  npm install ast-grep failed"
        else echo "  ⚠️  Install ast-grep manually: https://ast-grep.github.io/  (brew install ast-grep)"; fi
    fi
    if ! uhave rg; then
        echo "• ripgrep missing — installing..."
        if [ -n "$BREW" ]; then as_user "$BREW" install ripgrep || echo "  ⚠️  brew install ripgrep failed"
        else echo "  ⚠️  Install ripgrep manually: brew install ripgrep"; fi
    fi
fi

# ── obtain the binary: local build -> cargo build -> download prebuilt ────────
case "$(uname -m)" in
    arm64|aarch64) ASSET="smart-rg-macos-arm64" ;;
    x86_64)        ASSET="smart-rg-macos-x86_64" ;;
    *)             ASSET="" ;;
esac

if [ -f "./target/release/smart-rg" ]; then
    BIN="./target/release/smart-rg"
elif [ -f "Cargo.toml" ]; then
    if ! uhave cargo; then
        if [ "$INSTALL_DEPS" -eq 1 ]; then
            echo "• Rust/cargo missing — installing rustup..."
            ulc "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y" \
              || { echo "❌ rustup install failed. Install Rust manually: https://rustup.rs"; exit 1; }
        else
            echo "❌ cargo not found and --no-deps set. Install Rust: https://rustup.rs"; exit 1
        fi
    fi
    echo "Building from source (cargo build --release)..."
    ulc 'source "$HOME/.cargo/env" 2>/dev/null; cargo build --release'
    BIN="./target/release/smart-rg"
elif [ -n "$ASSET" ] && command -v curl >/dev/null 2>&1; then
    echo "No source checkout — downloading prebuilt $ASSET ..."
    DL="$(mktemp)"
    if curl -fsSL "https://github.com/$REPO/releases/latest/download/$ASSET" -o "$DL" && [ -s "$DL" ]; then
        chmod +x "$DL"; BIN="$DL"; echo "✓ Downloaded prebuilt binary"
    else
        rm -f "$DL"
        echo "❌ Could not download prebuilt binary. Clone the repo and build from source."
        exit 1
    fi
else
    echo "❌ No binary, no source, no prebuilt for arch '$(uname -m)'."; exit 1
fi
echo "✓ Binary: $BIN"

# ── migrate old installs, then install into the dedicated dir ──────────────────
migrate_old_shim

mkdir -p "$SRG_BIN"
cp "$BIN" "$SRG_BIN/rg"
chmod +x "$SRG_BIN/rg"
echo "✓ Installed shim → $SRG_BIN/rg"

# Also expose a `smart-rg` command (relative symlink to the same binary, which
# routes subcommands by argv) so the documented `smart-rg stats` / `report` work
# — the dedicated bin dir is already first on PATH.
ln -sf rg "$SRG_BIN/smart-rg"
echo "✓ Installed command → $SRG_BIN/smart-rg"

if real="$(resolve_real_rg)"; then
    ln -sf "$real" "$SRG_BIN/rg2"
    echo "🔗 Linked real ripgrep  $SRG_BIN/rg2 → $real"
else
    echo "⚠️  Real ripgrep not found. Install ripgrep and re-run, or create"
    echo "    $SRG_BIN/rg2 → your rg manually (the shim needs it to forward)."
fi

write_env_sh

while IFS= read -r f; do
    strip_block "$f"   # clean legacy/duplicate first (keeps install idempotent)
    add_block "$f"
done < <(target_files)

# ── configure Claude Code (it uses a BUNDLED rg unless USE_BUILTIN_RIPGREP=0) ──
if [ "$CONFIGURE_CLAUDE" -eq 1 ]; then
    merge_claude_setting "$REAL_HOME/.claude/settings.json" || true
    [ "$EUID" -eq 0 ] && chown -R "$REAL_USER" "$REAL_HOME/.claude" 2>/dev/null || true
fi

# ownership fixups when run under sudo
[ "$EUID" -eq 0 ] && chown -R "$REAL_USER" "$SRG_HOME" 2>/dev/null || true

uhave ast-grep || echo "  ⚠️  ast-grep not found — structural searches will fall back to text."

echo ""
self_verify

cat <<EOF

✅ smart-rg is ready.
   Open a new shell (or:  exec \$SHELL -l ) to pick up the PATH change.
   Verify:  command -v rg     # should show $SRG_BIN/rg
   See savings:  smart-rg stats
   HTML report:  smart-rg report -o report.html --open
EOF
