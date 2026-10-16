#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# 46Anarchy Launcher - Linux installer.
#
# Installs the right release bundle for the running distro from the latest
# GitHub release of 46Anarchy/46Launcher:
#   Debian/Ubuntu family -> .deb      (apt-get, fallback dpkg)
#   Fedora/RHEL family    -> .rpm      (dnf, fallback rpm)
#   Arch / openSUSE / rest -> .AppImage (fallback, installed user-local)
#
# Usage:
#   ./install.sh [--dry-run] [-h|--help]
#   curl -fsSL https://raw.githubusercontent.com/46Anarchy/46Launcher/master/install.sh | bash
#
# Env knobs:
#   GITHUB_REPO      release repo, default "46Anarchy/46Launcher".
#   GITHUB_TOKEN     optional, sent as Authorization header (rate limits).
#   INSTALL_BIN_DIR  AppImage target dir, default "$HOME/.local/bin".
#   APP_DIR          desktop entry dir, default "$HOME/.local/share/applications".
#   ICON_DIR         icon target dir, default
#                    "$HOME/.local/share/icons/hicolor/512x512/apps".
#   ARCH_VARIANT     microarch preference: "auto" (default: pick x86-64-v3
#                    when the CPU has all required flags, else baseline),
#                    "v3" (always prefer the -v3 asset), or "baseline".
# Test hooks (not for normal use):
#   PROC_CPUINFO     cpuinfo source, default "/proc/cpuinfo".
#   FAKE_MACHINE     overrides `uname -m`.
#   INSTALL_SH_LIB=1 source this file for its functions only (no install).
#
# Dependencies: only `curl` (+ `sudo` when not running as root).

set -euo pipefail

# --- Configuration (defaults, overridable via env) ---------------------------
GITHUB_REPO="${GITHUB_REPO:-46Anarchy/46Launcher}"
INSTALL_BIN_DIR="${INSTALL_BIN_DIR:-$HOME/.local/bin}"
APP_DIR="${APP_DIR:-$HOME/.local/share/applications}"
ICON_DIR="${ICON_DIR:-$HOME/.local/share/icons/hicolor/512x512/apps}"
APPIMAGE_NAME="46anarchy.AppImage"
DESKTOP_NAME="46anarchy.desktop"
ICON_NAME="46anarchy.png"
# Branch serving icon.png via raw.githubusercontent.com (keep in sync).
ICON_BRANCH="master"
# Microarch preference: auto | v3 | baseline (see header).
ARCH_VARIANT="${ARCH_VARIANT:-auto}"
# cpuinfo source (overridable for tests).
PROC_CPUINFO="${PROC_CPUINFO:-/proc/cpuinfo}"
# x86-64-v3 ISA gate: AVX/AVX2/BMI1-2/FMA/LZCNT/MOVBE/OSXSAVE
# (~2013+ CPUs: Intel Haswell+, AMD Excavator / Zen+ and newer).
V3_REQUIRED_FLAGS="avx avx2 bmi1 bmi2 fma lzcnt movbe osxsave"

DRY_RUN=0

# --- CLI flags ---------------------------------------------------------------
usage() {
    printf '%s\n' 'Usage: install.sh [--dry-run] [-h|--help]'
    printf '%s\n' ''
    printf '%s\n' 'Installs the 46Anarchy launcher bundle matching this distro.'
    printf '%s\n' '--dry-run prints every decision and command without executing.'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 1 ;;
    esac
done

# --- Helpers -----------------------------------------------------------------
# Print an informational line.
info() {
    printf '%s\n' "$*"
}

# Abort with an error message.
die() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

# --- Privilege handling ------------------------------------------------------
# System packages (deb/rpm) need root: use sudo when not root, run directly
# as root, and fail clearly when neither is possible.
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
else
    if ! command -v sudo >/dev/null 2>&1; then
        die "this installer needs root privileges for system packages but 'sudo' was not found. Re-run as root."
    fi
    SUDO="sudo"
fi

# --- Dependency check --------------------------------------------------------
# Only hard dependency is curl (sudo handled above).
if ! command -v curl >/dev/null 2>&1; then
    die "'curl' is required but not installed. Install curl and re-run."
fi

# --- Distro detection --------------------------------------------------------
# Read ID and ID_LIKE from /etc/os-release, lowercase them, and match each
# whitespace-separated token against the known families. Anything unmatched
# falls back to AppImage (works on every distro).
# Sets globals: os_id, os_like (raw tokens), FORMAT (deb|rpm|appimage) and
# reason (human-readable explanation). Assigns globals instead of echoing so
# callers keep the explanation text (command substitution would trap it in a
# subshell and lose it under `set -u`).
detect_format() {
    os_id=""
    os_like=""
    FORMAT="appimage"
    reason="no /etc/os-release match (or file missing), using portable fallback"
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        os_id="$(grep -m 1 -E '^ID=' /etc/os-release | cut -d= -f2 | tr -d '"' | tr '[:upper:]' '[:lower:]')"
        os_like="$(grep -m 1 -E '^ID_LIKE=' /etc/os-release | cut -d= -f2 | tr -d '"' | tr '[:upper:]' '[:lower:]')"
    fi

    # shellcheck disable=SC2086
    for token in $os_id $os_like; do
        case "$token" in
            debian|ubuntu|linuxmint|pop|elementary|zorin|kali|raspbian)
                FORMAT="deb"
                reason="distro token '$token' belongs to the Debian family"
                break
                ;;
            fedora|rhel|centos|rocky|alma)
                FORMAT="rpm"
                reason="distro token '$token' belongs to the Fedora/RHEL family"
                break
                ;;
            arch|manjaro|garuda|endeavouros|opensuse*|suse|tumbleweed)
                FORMAT="appimage"
                reason="distro token '$token' has no native package, using portable fallback"
                break
                ;;
        esac
    done
}

# --- Microarch variant detection ---------------------------------------------
# First "flags" line of $PROC_CPUINFO, lowercased; empty when unreadable.
read_cpu_flags() {
    [ -r "$PROC_CPUINFO" ] || return 0
    grep -m 1 -i '^flags' "$PROC_CPUINFO" | cut -d: -f2- | tr '[:upper:]' '[:lower:]' || true
}

# Echo the subset of V3_REQUIRED_FLAGS absent from $1 (exact,
# case-insensitive-by-caller tokens); empty output means v3-capable.
v3_missing_flags() {
    local padded=" $1 " f missing=""
    # shellcheck disable=SC2086
    for f in $V3_REQUIRED_FLAGS; do
        case "$padded" in
            *" $f "*) ;;
            *) missing="$missing $f" ;;
        esac
    done
    printf '%s\n' "${missing# }"
}

# 0 iff every V3_REQUIRED_FLAGS token occurs in $1 as an exact token
# (padding both sides, so e.g. "noavx" never counts as "avx").
cpu_has_v3_flags() {
    [ -z "$(v3_missing_flags "$1")" ]
}

# Sets globals VARIANT (v3|baseline), variant_reason, MACHINE, CPU_FLAGS.
# ARCH_VARIANT=v3|baseline forces the choice; auto picks v3 only on x86_64
# with all V3_REQUIRED_FLAGS present, else baseline (never fatal: unknown
# arch or unreadable flags fall back to baseline, the max-compat build).
detect_arch_variant() {
    MACHINE="${FAKE_MACHINE:-$(uname -m)}"
    case "$ARCH_VARIANT" in
        v3)
            VARIANT="v3"
            variant_reason="ARCH_VARIANT=v3 override"
            return 0
            ;;
        baseline)
            VARIANT="baseline"
            variant_reason="ARCH_VARIANT=baseline override"
            return 0
            ;;
        auto|"") ;;
        *) die "ARCH_VARIANT must be v3, baseline, or auto (got '$ARCH_VARIANT')." ;;
    esac
    if [ "$MACHINE" != "x86_64" ]; then
        VARIANT="baseline"
        variant_reason="machine arch '$MACHINE' is not x86_64"
        return 0
    fi
    CPU_FLAGS="$(read_cpu_flags)"
    if [ -z "${CPU_FLAGS:-}" ]; then
        VARIANT="baseline"
        variant_reason="no CPU flags readable from $PROC_CPUINFO"
        return 0
    fi
    missing="$(v3_missing_flags "$CPU_FLAGS")"
    if [ -z "$missing" ]; then
        VARIANT="v3"
        variant_reason="all x86-64-v3 flags present"
    else
        VARIANT="baseline"
        variant_reason="missing x86-64-v3 flag(s): $missing"
    fi
}

# Echo the release-asset URL for .$2 in the JSON on $1: when $3 is v3,
# prefer the `-v3` asset (e.g. ..._amd64-v3.deb) and fall back to the plain
# baseline asset when the release has none (older releases); otherwise take
# the first non-v3 match (a plain ".ext" grep would also catch -v3 names).
# Empty output when nothing matches.
pick_asset_url() {
    json="$1"; ext="$2"; variant="$3"
    url=""
    if [ "$variant" = "v3" ]; then
        url="$(printf '%s\n' "$json" \
            | grep -o '"browser_download_url": *"[^"]*"' \
            | grep -m 1 -- "-v3\\.${ext}\"" \
            | sed 's/.*"\(https[^"]*\)".*/\1/' || true)"
    fi
    if [ -z "$url" ]; then
        url="$(printf '%s\n' "$json" \
            | grep -o '"browser_download_url": *"[^"]*"' \
            | grep -- "\\.${ext}\"" | grep -v -- "-v3\\." \
            | head -n 1 \
            | sed 's/.*"\(https[^"]*\)".*/\1/' || true)"
    fi
    printf '%s\n' "$url"
}

# Sourcing seam for tests/install.sh unit checks (see tests/):
# with INSTALL_SH_LIB=1 only the functions above are loaded.
if [ "${INSTALL_SH_LIB:-0}" = "1" ]; then
    return 0 2>/dev/null || exit 0
fi

detect_format
detect_arch_variant
API_URL="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"

# File extension wanted for the detected format (Tauri emits ".AppImage").
case "$FORMAT" in
    deb) WANT_EXT="deb" ;;
    rpm) WANT_EXT="rpm" ;;
    *)   WANT_EXT="AppImage" ;;
esac

# --- Dry run -----------------------------------------------------------------
# Print every decision and every command that a real run would execute, then
# exit before any network or filesystem side effect.
if [ "$DRY_RUN" -eq 1 ]; then
    info "[dry-run] Detected OS: ID='${os_id:-unknown}' ID_LIKE='${os_like:-none}'"
    info "[dry-run] Selected format: .${WANT_EXT} (${reason})"
    info "[dry-run] Machine arch: ${MACHINE} (ARCH_VARIANT=${ARCH_VARIANT})"
    if [ "$MACHINE" = "x86_64" ] && [ "${ARCH_VARIANT}" != "baseline" ] && [ "${ARCH_VARIANT}" != "v3" ]; then
        info "[dry-run] CPU flags source: ${PROC_CPUINFO}"
        info "[dry-run] CPU flags:${CPU_FLAGS:- <unavailable>}"
    fi
    info "[dry-run] Microarch variant: ${VARIANT} (${variant_reason})"
    if [ "$VARIANT" = "v3" ]; then
        info "[dry-run] Would prefer the '-v3' asset (*-v3.${WANT_EXT}), falling back to the baseline asset when the release has none."
    else
        info "[dry-run] Would pick the baseline asset (*.${WANT_EXT}, ignoring any -v3 files)."
    fi
    info "[dry-run] Release repo: ${GITHUB_REPO}"
    info "[dry-run] Would query latest release metadata:"
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        info "[dry-run]   + curl -fsSL -H 'Authorization: Bearer <redacted>' '${API_URL}'"
    else
        info "[dry-run]   + curl -fsSL '${API_URL}'"
    fi
    info "[dry-run] Would pick the download URL with pick_asset_url (grep/sed, no jq)."
    info "[dry-run] Would download the asset into a fresh temp dir (mktemp -d, trap cleanup)."
    case "$FORMAT" in
        deb)
            info "[dry-run] Would install with:"
            info "[dry-run]   + ${SUDO:+$SUDO }apt-get install -y ./<asset>.${WANT_EXT}"
            info "[dry-run]   (fallback on failure: ${SUDO:+$SUDO }dpkg -i ./<asset>.${WANT_EXT})"
            ;;
        rpm)
            info "[dry-run] Would install with:"
            info "[dry-run]   + ${SUDO:+$SUDO }dnf install -y ./<asset>.${WANT_EXT}"
            info "[dry-run]   (fallback on failure: ${SUDO:+$SUDO }rpm -Uvh ./<asset>.${WANT_EXT})"
            ;;
        *)
            info "[dry-run] Would install the AppImage user-locally with:"
            info "[dry-run]   + mkdir -p '${INSTALL_BIN_DIR}'"
            info "[dry-run]   + cp ./<asset>.${WANT_EXT} '${INSTALL_BIN_DIR}/${APPIMAGE_NAME}'"
            info "[dry-run]   + chmod +x '${INSTALL_BIN_DIR}/${APPIMAGE_NAME}'"
            info "[dry-run]   + write '${APP_DIR}/${DESKTOP_NAME}' (Name=46Anarchy, Exec=${INSTALL_BIN_DIR}/${APPIMAGE_NAME} %U, Icon=46anarchy, ...)"
            info "[dry-run]   + curl -fsSL 'https://raw.githubusercontent.com/${GITHUB_REPO}/${ICON_BRANCH}/icon.png' -o '${ICON_DIR}/${ICON_NAME}'"
            info "[dry-run]   + update-desktop-database (only if present)"
            info "[dry-run]   + gtk-update-icon-cache (only if present)"
            ;;
    esac
    info "[dry-run] No changes made."
    exit 0
fi

# --- Resolve the asset URL ---------------------------------------------------
# Query the GitHub Releases API for the latest release and extract the first
# browser_download_url ending in the wanted extension. grep/sed only: zero
# extra dependencies. GITHUB_TOKEN is honored when set (rate limits) and the
# request works without it.
info "Fetching latest release metadata for ${GITHUB_REPO}..."
curl_args=(-fsSL)
if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl_args+=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
fi
release_json="$(curl "${curl_args[@]}" "$API_URL")"

ASSET_URL="$(pick_asset_url "$release_json" "$WANT_EXT" "$VARIANT")"

if [ -z "${ASSET_URL:-}" ]; then
    die "no .${WANT_EXT} asset found in the latest ${GITHUB_REPO} release (${API_URL})."
fi
info "Selected asset: ${ASSET_URL}"
if [ "$VARIANT" = "v3" ]; then
    case "$ASSET_URL" in
        *"-v3.${WANT_EXT}") ;;
        *) info "No -v3 asset in this release, using the baseline build instead." ;;
    esac
fi

# --- Download ----------------------------------------------------------------
# Everything lands in a temp dir that is always cleaned up on exit.
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
ASSET_FILE="${TMP_DIR}/$(basename "$ASSET_URL")"
info "Downloading to ${ASSET_FILE}..."
curl -fsSL "$ASSET_URL" -o "$ASSET_FILE"

# --- Install: deb ------------------------------------------------------------
install_deb() {
    info "Installing .deb package..."
    if command -v apt-get >/dev/null 2>&1; then
        # shellcheck disable=SC2086
        if ! $SUDO apt-get install -y "$ASSET_FILE"; then
            info "apt-get failed, falling back to dpkg."
            # shellcheck disable=SC2086
            $SUDO dpkg -i "$ASSET_FILE"
        fi
    else
        # shellcheck disable=SC2086
        $SUDO dpkg -i "$ASSET_FILE"
    fi
}

# --- Install: rpm ------------------------------------------------------------
install_rpm() {
    info "Installing .rpm package..."
    if command -v dnf >/dev/null 2>&1; then
        # shellcheck disable=SC2086
        if ! $SUDO dnf install -y "$ASSET_FILE"; then
            info "dnf failed, falling back to rpm."
            # shellcheck disable=SC2086
            $SUDO rpm -Uvh "$ASSET_FILE"
        fi
    else
        # shellcheck disable=SC2086
        $SUDO rpm -Uvh "$ASSET_FILE"
    fi
}

# --- Install: AppImage -------------------------------------------------------
# Portable fallback: the AppImage ships no .desktop entry, so one is written
# here along with the icon fetched from the repo (master branch).
install_appimage() {
    target="${INSTALL_BIN_DIR}/${APPIMAGE_NAME}"
    desktop_file="${APP_DIR}/${DESKTOP_NAME}"
    icon_file="${ICON_DIR}/${ICON_NAME}"
    icon_url="https://raw.githubusercontent.com/${GITHUB_REPO}/${ICON_BRANCH}/icon.png"

    info "Installing AppImage to ${target}..."
    mkdir -p "$INSTALL_BIN_DIR"
    cp "$ASSET_FILE" "$target"
    chmod +x "$target"

    info "Writing desktop entry to ${desktop_file}..."
    mkdir -p "$APP_DIR"
    cat > "$desktop_file" <<EOF
[Desktop Entry]
Name=46Anarchy
Exec=${target} %U
Icon=46anarchy
Type=Application
Categories=Game;
Terminal=false
StartupNotify=true
EOF

    info "Fetching icon to ${icon_file}..."
    mkdir -p "$ICON_DIR"
    curl -fsSL "$icon_url" -o "$icon_file"

    # Refresh desktop/icon caches opportunistically: never fail the install.
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t "$(dirname "$(dirname "$ICON_DIR")")" || true
    fi
}

# --- Dispatch ----------------------------------------------------------------
case "$FORMAT" in
    deb) install_deb ;;
    rpm) install_rpm ;;
    *)   install_appimage ;;
esac

info "46Anarchy installed successfully."
