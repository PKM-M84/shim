#!/bin/bash
set -e

REPO="PKM-M84/shim-"

# ── helpers ───────────────────────────────────────────────────────────────────
# Run user-space tools (brew, rustup, cargo) as the REAL user even under sudo,
# and elevate only the steps that truly need root (/usr/local/bin writes).
REAL_USER="${SUDO_USER:-$(whoami)}"
REAL_HOME="$(eval echo "~$REAL_USER")"
as_user()   { if [ "$EUID" -eq 0 ] && [ "$REAL_USER" != "root" ]; then sudo -u "$REAL_USER" -H "$@"; else "$@"; fi; }
need_root() { if [ "$EUID" -eq 0 ]; then "$@"; else sudo "$@"; fi; }
uhave()     { as_user bash -lc "command -v '$1'" >/dev/null 2>&1; }   # is <cmd> on the real user's PATH?
ulc()       { as_user bash -lc "$1"; }                                # run a command line as the real user

# If something other than the shim's rg is first on PATH (classic case: Homebrew's
# rg on Apple Silicon, where /opt/homebrew/bin precedes /usr/local/bin), prepend
# /usr/local/bin in the user's shell profile so the shim wins. Idempotent.
fix_path_if_needed() {
    [ "${FIX_PATH:-1}" -eq 1 ] || return 0
    [ "$(command -v rg 2>/dev/null)" = "/usr/local/bin/rg" ] && return 0

    # Strategy 1 (preferred, non-root): symlink rg into the first user-writable
    # directory that's ALREADY ahead of the real rg on PATH. Takes effect with
    # just `hash -r` — no profile edit, no terminal restart. (Skipped under sudo,
    # where -w and $PATH reflect root, not the user.)
    if [ "$EUID" -ne 0 ]; then
        local IFS=':' dirs d rgdir
        read -ra dirs <<< "$PATH"
        rgdir="$(dirname "$(command -v rg 2>/dev/null)" 2>/dev/null)"
        for d in "${dirs[@]}"; do
            [ -n "$d" ] || continue
            [ "$d" = "$rgdir" ] && break          # reached the real rg — stop
            [ "$d" = "/usr/local/bin" ] && continue
            if [ -d "$d" ] && [ -w "$d" ]; then
                if ln -sf /usr/local/bin/smart-rg "$d/rg"; then
                    echo "  ✓ Linked rg into $d (already ahead of Homebrew on your PATH)"
                    echo "    → run 'hash -r' (or open a new terminal), then: which rg"
                    return 0
                fi
            fi
        done
    fi

    # Strategy 2 (fallback): prepend /usr/local/bin in the shell profile.
    local rshell prof
    rshell="$(as_user bash -lc 'echo $SHELL' 2>/dev/null || echo /bin/zsh)"
    case "$rshell" in
        */bash) prof="$REAL_HOME/.bash_profile" ;;
        *)      prof="$REAL_HOME/.zshrc" ;;
    esac
    if [ -f "$prof" ] && grep -q 'added by smart-rg installer' "$prof" 2>/dev/null; then
        echo "  ℹ️  PATH fix already in $prof — restart your terminal (or: source $prof)"
        return 0
    fi
    printf '\n# added by smart-rg installer: put the shim (/usr/local/bin) before Homebrew rg\nexport PATH="/usr/local/bin:$PATH"\n' >> "$prof"
    [ "$EUID" -eq 0 ] && chown "$REAL_USER" "$prof" 2>/dev/null || true
    echo "  ✓ Prepended /usr/local/bin to PATH in $prof"
    echo "    → restart your terminal, or run:  source $prof   (then: which rg)"
}

# ── Claude Code settings merge (testable, no root, no install needed) ─────────
# Sets env.USE_BUILTIN_RIPGREP="0" in the given settings.json while preserving
# everything else. Honors SMARTRG_JSON_ENGINE=jq|python3|auto (default: auto)
# so the smoke test can exercise each code path. Returns non-zero if no JSON
# tool is available.
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

echo "🪶 smart-rg installer"
echo ""

# Flags:
#   --with-grep         also symlink `grep` -> shim (advanced; shadows system grep)
#   --no-claude-config  don't touch ~/.claude/settings.json
#   --no-deps           don't auto-install ast-grep / ripgrep / Rust
WITH_GREP=0
CONFIGURE_CLAUDE=1
INSTALL_DEPS=1
FIX_PATH=1
for arg in "$@"; do
  case "$arg" in
    --with-grep) WITH_GREP=1 ;;
    --no-claude-config) CONFIGURE_CLAUDE=0 ;;
    --no-deps) INSTALL_DEPS=0 ;;
    --no-fix-path) FIX_PATH=0 ;;
    -h|--help)
      echo "Usage: ./install.sh [--with-grep] [--no-claude-config] [--no-deps] [--no-fix-path]"
      echo "  (run as a normal user; it uses sudo only for /usr/local/bin)"
      echo "  --with-grep             also intercept 'grep' (shadows system grep; off by default)"
      echo "  --no-claude-config      leave ~/.claude/settings.json untouched"
      echo "  --no-deps               skip auto-installing ast-grep / ripgrep / Rust"
      echo "  --no-fix-path           don't adjust PATH if another rg shadows the shim"
      echo "  --check                 report what's installed and exit (no changes)"
      echo "  --merge-claude-config [path]   only set USE_BUILTIN_RIPGREP=0 and exit (no root)"
      exit 0 ;;
  esac
done

if [ "$EUID" -eq 0 ] && [ "$REAL_USER" = "root" ]; then
    echo "⚠️  Running as real root: Homebrew/rustup can't install as root."
    echo "    Run as a normal user instead (the script sudo's only for /usr/local/bin)."
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
    # building from a source checkout: ensure Rust, then build (as the user)
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

# ── install + symlink (needs root) ────────────────────────────────────────────
need_root cp "$BIN" /usr/local/bin/smart-rg
need_root ln -sf /usr/local/bin/smart-rg /usr/local/bin/rg
echo "✓ Installed: /usr/local/bin/smart-rg  (rg -> smart-rg)"

if [ "$WITH_GREP" -eq 1 ]; then
    need_root ln -sf /usr/local/bin/smart-rg /usr/local/bin/grep
    echo "⚠️  grep -> smart-rg  (system grep shadowed everywhere; undo: sudo rm /usr/local/bin/grep)"
fi

# ── configure Claude Code (it uses a BUNDLED rg unless USE_BUILTIN_RIPGREP=0) ──
if [ "$CONFIGURE_CLAUDE" -eq 1 ]; then
    merge_claude_setting "$REAL_HOME/.claude/settings.json" || true
    [ "$EUID" -eq 0 ] && chown -R "$REAL_USER" "$REAL_HOME/.claude" 2>/dev/null || true
fi

# ── verify ────────────────────────────────────────────────────────────────────
echo ""
echo "Verifying..."
echo "  rg → $(command -v rg 2>/dev/null || echo 'NOT FOUND')"
if [ "$(command -v rg 2>/dev/null)" = "/usr/local/bin/rg" ]; then
    echo "  ✓ shim is first on PATH"
elif [ "$FIX_PATH" -eq 1 ]; then
    echo "  • another rg is ahead on PATH — fixing so the shim wins:"
    fix_path_if_needed || true
else
    echo "  ⚠️  Another rg is first on PATH. Put /usr/local/bin (or ~/.local/bin) ahead of it,"
    echo "      e.g.: ln -sf /usr/local/bin/smart-rg ~/.local/bin/rg && hash -r"
fi
uhave ast-grep || echo "  ⚠️  ast-grep still not found — structural searches will fall back to text."

echo ""
echo "✅ smart-rg is ready."
echo "   See savings:  smart-rg stats"
echo "   HTML report:  smart-rg report -o report.html --open"
