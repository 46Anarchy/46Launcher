#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# install.sh microarch-variant checks: no dependencies beyond bash + coreutils.
# Covers detect_arch_variant (via --dry-run) and pick_asset_url (via the
# INSTALL_SH_LIB sourcing seam). Run: bash tests/test-install-variant.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

# assert $1 (haystack) contains $2 (needle).
assert_contains() {
    if [[ "$1" == *"$2"* ]]; then ok "$3"; else
        fail "$3 (missing '$2' in: $1)"
    fi
}

# Write a fake cpuinfo with the given flags line to stdout path $1.
make_cpuinfo() {
    printf 'processor\t: 0\nflags\t\t: %s\n' "$2" > "$1"
}

V3_FLAGS="avx avx2 bmi1 bmi2 fma lzcnt movbe osxsave"
FULL="fpu vme sse sse2 ssse3 sse4_1 sse4_2 popcnt aes xsave avx f16c rdrand abm fma movbe bmi1 avx2 bmi2 fma lzcnt osxsave avx512f avx512dq xsaveopt xsavec"

# --- dry-run decision matrix -------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 1. all v3 flags present -> v3, prefers -v3 asset.
make_cpuinfo "$TMP/full.cpu" "$FULL"
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: v3" "auto+all-flags selects v3"
assert_contains "$out" "Would prefer the '-v3' asset" "v3 dry-run prefers -v3 asset"

# 2. v4-ish superset (extra AVX512 flags) still v3.
make_cpuinfo "$TMP/v4.cpu" "$FULL avx512bw avx512vl avx512vbmi"
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/v4.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: v3" "auto+superset selects v3"

# 3. each single missing flag -> baseline naming that flag.
for drop in $V3_FLAGS; do
    subset=""
    for f in $FULL; do
        [ "$f" = "$drop" ] || subset="$subset $f"
    done
    make_cpuinfo "$TMP/miss.cpu" "$subset"
    out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/miss.cpu" bash "$INSTALL_SH" --dry-run)"
    assert_contains "$out" "Microarch variant: baseline" "auto+missing-$drop selects baseline"
    assert_contains "$out" "$drop" "auto+missing-$drop names $drop"
done

# 4. flag matching is case-insensitive.
make_cpuinfo "$TMP/upper.cpu" "$(printf '%s' "$FULL" | tr '[:lower:]' '[:upper:]')"
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/upper.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: v3" "uppercase flags select v3"

# 5. exact tokens only: "noavx"/"avx2x" must not count as avx/avx2.
make_cpuinfo "$TMP/sub.cpu" "noavx avx2x bmi1 bmi2 fma lzcnt movbe osxsave avx2"
# (avx missing: only noavx/avx2x present; avx2 present via exact token)
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/sub.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: baseline" "substring flags do not count"
assert_contains "$out" "missing x86-64-v3 flag(s): avx" "substring case names avx missing"

# 6. non-x86_64 (aarch64) with full flags -> baseline.
out="$(ARCH_VARIANT=auto FAKE_MACHINE=aarch64 PROC_CPUINFO="$TMP/full.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: baseline" "aarch64 selects baseline"
assert_contains "$out" "is not x86_64" "aarch64 reason mentions arch"

# 7. ARCH_VARIANT overrides.
make_cpuinfo "$TMP/miss2.cpu" "sse sse2 avx"
out="$(ARCH_VARIANT=v3 FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/miss2.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: v3" "ARCH_VARIANT=v3 forces v3"
assert_contains "$out" "ARCH_VARIANT=v3 override" "v3 override reason shown"
out="$(ARCH_VARIANT=baseline FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: baseline" "ARCH_VARIANT=baseline forces baseline"
if ARCH_VARIANT=bogus FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" bash "$INSTALL_SH" --dry-run >/dev/null 2>&1; then
    fail "invalid ARCH_VARIANT exits nonzero"
else
    ok "invalid ARCH_VARIANT exits nonzero"
fi

# 8. unreadable cpuinfo -> baseline, never fatal.
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/does-not-exist" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Microarch variant: baseline" "missing cpuinfo selects baseline"
assert_contains "$out" "no CPU flags readable" "missing cpuinfo reason shown"

# --- distro format matrix (atomic desktops -> AppImage) -----------------------
# NOTE: every case pins OSTREE_BOOTED at a missing path, except the marker
# case — otherwise this host's real /run/ostree-booted would force AppImage.
make_os_release() {
    printf 'ID=%s\nID_LIKE=%s\n' "$2" "$3" > "$1"
}

for spec in \
    "bazzite|bazzite|fedora" \
    "silverblue|silverblue|fedora" \
    "aurora|aurora|fedora" \
    "aeon|aeon|suse opensuse" \
    "microos|opensuse-microos|suse opensuse" \
    "steamos|steamos|arch" \
    "vanilla|vanilla|ubuntu debian" \
; do
    name="${spec%%|*}"; rest="${spec#*|}"; id="${rest%%|*}"; like="${rest#*|}"
    make_os_release "$TMP/$name.os" "$id" "$like"
    out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" OS_RELEASE="$TMP/$name.os" OSTREE_BOOTED="$TMP/no-marker" bash "$INSTALL_SH" --dry-run)"
    assert_contains "$out" "Selected format: .AppImage" "atomic $name selects AppImage"
done

# ostree marker wins even over a native-packaging ID (Ubuntu + marker).
touch "$TMP/ostree-marker"
make_os_release "$TMP/ubuntu.os" "ubuntu" "debian"
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" OS_RELEASE="$TMP/ubuntu.os" OSTREE_BOOTED="$TMP/ostree-marker" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Selected format: .AppImage" "ostree marker forces AppImage on ubuntu"
assert_contains "$out" "ostree" "ostree reason shown"

# natives still resolve without marker or atomic IDs (regression guards).
make_os_release "$TMP/fedora.os" "fedora" ""
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" OS_RELEASE="$TMP/fedora.os" OSTREE_BOOTED="$TMP/no-marker" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Selected format: .rpm" "plain fedora still selects rpm"
out="$(ARCH_VARIANT=auto FAKE_MACHINE=x86_64 PROC_CPUINFO="$TMP/full.cpu" OS_RELEASE="$TMP/ubuntu.os" OSTREE_BOOTED="$TMP/no-marker" bash "$INSTALL_SH" --dry-run)"
assert_contains "$out" "Selected format: .deb" "plain ubuntu still selects deb"

# --- pick_asset_url units ----------------------------------------------------
JSON_BOTH='{"assets": [
  {"browser_download_url": "https://example.com/46Anarchy_0.1.0_amd64.deb"},
  {"browser_download_url": "https://example.com/46Anarchy_0.1.0_amd64-v3.deb"}]}'
JSON_BASE_ONLY='{"assets": [
  {"browser_download_url": "https://example.com/46Anarchy_0.1.0_amd64.deb"}]}'
JSON_V3_FIRST='{"assets": [
  {"browser_download_url": "https://example.com/46Anarchy_0.1.0_amd64-v3.deb"},
  {"browser_download_url": "https://example.com/46Anarchy_0.1.0_amd64.deb"}]}'
JSON_RPM='{"assets": [
  {"browser_download_url": "https://example.com/46Anarchy-0.1.0-1.x86_64.rpm"},
  {"browser_download_url": "https://example.com/46Anarchy-0.1.0-1.x86_64-v3.rpm"}]}'

# shellcheck disable=SC1090
pick() { INSTALL_SH_LIB=1 INSTALL_SH="$INSTALL_SH" bash -c 'json="$0"; ext="$1"; variant="$2"; set --; source "$INSTALL_SH"; pick_asset_url "$json" "$ext" "$variant"' "$1" "$2" "$3"; }

got="$(pick "$JSON_BOTH" deb v3)"
[ "$got" = "https://example.com/46Anarchy_0.1.0_amd64-v3.deb" ] \
    && ok "v3 picks -v3 deb" || fail "v3 picks -v3 deb (got: $got)"
got="$(pick "$JSON_BASE_ONLY" deb v3)"
[ "$got" = "https://example.com/46Anarchy_0.1.0_amd64.deb" ] \
    && ok "v3 falls back to baseline deb" || fail "v3 falls back to baseline deb (got: $got)"
got="$(pick "$JSON_V3_FIRST" deb baseline)"
[ "$got" = "https://example.com/46Anarchy_0.1.0_amd64.deb" ] \
    && ok "baseline ignores -v3 deb" || fail "baseline ignores -v3 deb (got: $got)"
got="$(pick "$JSON_RPM" rpm v3)"
[ "$got" = "https://example.com/46Anarchy-0.1.0-1.x86_64-v3.rpm" ] \
    && ok "v3 picks -v3 rpm" || fail "v3 picks -v3 rpm (got: $got)"
got="$(pick '{"assets": []}' deb v3)"
[ -z "$got" ] && ok "no match yields empty" || fail "no match yields empty (got: $got)"

# cpu_has_v3_flags unit via the same seam.
lib_check() { INSTALL_SH_LIB=1 INSTALL_SH="$INSTALL_SH" bash -c 'flags="$0"; set --; source "$INSTALL_SH"; cpu_has_v3_flags "$flags"' "$1"; }
lib_check "$FULL" && ok "cpu_has_v3_flags true on full set" || fail "cpu_has_v3_flags true on full set"
lib_check "sse avx" && fail "cpu_has_v3_flags false on partial" || ok "cpu_has_v3_flags false on partial"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
