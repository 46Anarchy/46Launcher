<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# 46Anarchy Launcher

Native launcher for the 46Anarchy Paladium modpack (Minecraft 1.7.10, Forge),
written in Rust with a Tauri v2 shell and a vanilla HTML/CSS/JS frontend
(no npm, no build step for the UI). It fetches and verifies all modpack files
at install time from the CDN manifest, then launches the game. There is no
offline mode: login with a Microsoft account is mandatory.

## Features

- **Microsoft auth (device-code flow):** MSA → Xbox Live → XSTS →
   `login_with_xbox` → entitlements check → profile. Only the MSA
   `refresh_token` is persisted (OS keyring, service `fr.46anarchy.launcher`,
   one entry per account UUID); the Minecraft `access_token` stays in memory.
   Sessions older than ~20h are refreshed silently before launch.

## Install

One-liner (Linux):

```sh
curl -fsSL https://raw.githubusercontent.com/46Anarchy/46Launcher/master/install.sh | bash
```

`install.sh` detects the distro from `/etc/os-release`, fetches the latest
GitHub release, and installs the matching bundle. Only `curl` is required
(`sudo` when not root, for system packages).

| Distro family | Bundle installed | Method |
| --- | --- | --- |
| Debian, Ubuntu, Linux Mint, Pop!_OS, elementary OS, Zorin OS, Kali, Raspbian | `.deb` | `apt-get install` (fallback `dpkg -i`) |
| Fedora, RHEL, CentOS, Rocky Linux, AlmaLinux | `.rpm` | `dnf install` (fallback `rpm -Uvh`) |
| Arch, Manjaro, Garuda, EndeavourOS, openSUSE, anything else | `.AppImage` | copied to `~/.local/bin/46anarchy.AppImage` with a desktop entry and icon, no root needed |

Preview without changing anything:

```sh
curl -fsSL https://raw.githubusercontent.com/46Anarchy/46Launcher/master/install.sh | bash -s -- --dry-run
```

`GITHUB_REPO` overrides the release source, `GITHUB_TOKEN` is honored for
GitHub API rate limits, and `INSTALL_BIN_DIR` / `APP_DIR` override the
AppImage install paths (see the script header).

Manual alternative: download the `.deb`, `.rpm`, or `.AppImage` from
[Releases](https://github.com/46Anarchy/46Launcher/releases), then:

```sh
sudo apt-get install ./46anarchy_*.deb   # Debian/Ubuntu family
sudo dnf install ./46anarchy-*.rpm       # Fedora/RHEL family
chmod +x ./46Anarchy_*.AppImage          # anywhere: portable, no install step
```

The `.deb`/`.rpm` bundles ship their own `.desktop` entry; the `.AppImage`
does not (the installer script writes one for you).

Linux x86-64 builds come in two microarch variants: `baseline` (default
CPU target, runs anywhere) and `x86-64-v3` (AVX/AVX2/BMI1-2/FMA/LZCNT/
MOVBE/OSXSAVE — Intel Haswell (2013+) / AMD Excavator, Zen+ and newer —
faster number-crunching, refuses to boot on older CPUs). Release assets
carry a `-v3` infix before the arch
(`46Anarchy_0.1.0_amd64-v3.AppImage`, `..._amd64-v3.deb`,
`46Anarchy-0.1.0-1.x86_64-v3.rpm`); unmarked files are the baseline.
`install.sh` auto-picks: v3 when the first `flags` line of
`/proc/cpuinfo` contains every required flag, else baseline (non-x86_64
is always baseline), preferring the `-v3` asset and falling back to the
baseline one when a release has none. Override with
`ARCH_VARIANT=v3|baseline|auto` (default `auto`); `--dry-run` prints the
detected flags and the decision.

Uninstall:

```sh
sudo apt-get remove 46anarchy   # .deb
sudo dnf remove 46anarchy       # .rpm
# .AppImage: delete ~/.local/bin/46anarchy.AppImage,
# ~/.local/share/applications/46anarchy.desktop, and the
# ~/.local/share/icons/hicolor/512x512/apps/46anarchy.png icon.
```

Game data under `~/.46anarchy` is left behind by all of the above; delete it
manually for a full wipe.

### macOS

The `.icns` icon is baked into the `.app` bundle by Tauri at build time:
there is nothing to do manually. Open the `.dmg` from Releases, drag
46Anarchy into `/Applications`, and the icon appears automatically. For
unsigned builds, right-click → Open on first launch (Gatekeeper).
- **Multi-account:** connected accounts form a roster (UUID + name, no
  secrets) with an active-account pointer; switching a roster-only account
  refreshes its token on demand. Dead tokens quarantine the account
  (kept visible, reconnect instead of vanish) instead of deleting it.
- **Manifest install / verify / cancel:** fetches
  `https://cdn.46anarchy.fr/manifest.json`, SHA-1 verifies, downloads in
  parallel with retries, tracks freshness in `cdn_last.json`
  (`Init` / `Ok` / `OutOfDate` / `CdnFailure`). A wiped `libraries/` or
  deleted mods re-download on the next JOUER even at unchanged version.
  The Console "Annuler" button aborts a running install. Maintenance mode
  from the CDN disables Play with the maintenance message.
- **Mod manager:** manifest-listed mods are required and locked; other local
  `.jar` files toggle via a `.disabled` rename. Search plus
  All/Required/Optional/Disabled filters; counts reflect real disk state.
- **Auto Java runtimes:** Temurin 8 JRE auto-provisioned into `<gameDir>/java`
  when no Java 8 is found (cancellable); GraalVM 21 into `<gameDir>/java21`
  on the experimental path. A custom `java` binary override wins when set.
- **Pack flavors:** Modpack Paladium (Java 8, default) vs experimental
  modernized branch (Java 21 + LWJGL3). The experimental branch is currently
  **gated**: its entry stays visible but disabled in the install-options
  flavor picker ("bientôt disponible" with an explanatory tooltip),
  selection is refused by the frontend and by the backend
  (`set_pack_flavor(experimental=true)` returns a French error), and a
  stored experimental selection reverts to default once at boot with a
  French notice (nothing else is touched; the modern mod stack on disk is
  stashed, never deleted, by the existing play/install self-heal). The
  modern-stack code and its tests stay in the tree. Re-enable with the
  single `EXPERIMENTAL_GATED` switch (`crates/ui/src/state.rs`, mirrored in
  `crates/ui/frontend/dist/main.js`).
- **JVM settings applied at launch:** min/max memory, preset lines
  (`46anarchy` / `g1gc` / `lowram`, `graal-zgc` / `graal-lowram` on the
  experimental path, or custom text), resolution as `--width/--height`,
  `--fullscreen`. Unparsable values warn in the Console log. Log4Shell
  mitigation ships on every launch (`-Dlog4j2.formatMsgNoLookups=true` plus
  `JndiLookup.class` stripping from provisioned log4j-core jars).
- **Live console + crash surfacing:** game stdout/stderr streams into the
  Console tab when live console is on (lines capped at 500 chars, ring
  capped at 20000 lines) and tees to `<gameDir>/logs/launcher-console.log`;
  a post-launch watcher reports running / exited / crashed with the crash
  cause line in the journal and a toast.
- **Avatars:** per-UUID head renders fetched through the backend
  (minotar primary, mc-heads fallback), disk-cached 24h, served as `data:`
  URLs so the WebView never fetches remote images directly.
- **Patch notes:** local `<gameDir>/CHANGELOG.md` plus installed version
  plus FS-Patches GitHub releases, newest first.
- **Live server status:** Java Server List Ping against
  `mc.46anarchy.fr:25566` (players, version, MOTD, ping; TPS is not exposed
  by the protocol and shows as unavailable), refreshed every 30s.
- **External links open externally:** allowlisted `https://` hosts via a
  backend command (detached OS opener), never inside the WebView.
- **UI copy is French.** The Obsidian Kinetic theme (sidebar: Accueil / Mods /
  Configuration Java / Comptes / Console) is static CSS with locally vendored
  fonts (fully offline, zero CDN).

## Prerequisites

- **Rust stable via [rustup](https://rustup.rs/).** Plain `cargo build` /
  `cargo test` need nothing else (the `tauri` crate is optional behind
  `launcher-ui/tauri-backend`; `src-tauri` is workspace-excluded).
- **For `cargo tauri dev` / `cargo tauri build` only:** the
  [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/)
  (WebKitGTK 4.1 / GTK dev packages on Linux) plus
  `cargo install tauri-cli --version "^2.0.0" --locked`.
- **D-Bus Secret Service on Linux** for the `keyring` crate (e.g.
  `gnome-keyring` / `kwallet` with Secret Service enabled). Without it,
  refresh-token persistence fails (login itself still works in-memory).
- **Java 8** to actually launch the game — auto-downloaded on first run,
  so a manual install is optional.

## Development

The reference flow used here runs inside the `launcher-dev` distrobox
(which provides the system libraries), from the repo root:

```sh
distrobox enter launcher-dev
cargo test --workspace
```

Full check plus the Tauri harness check:

```sh
cargo test --workspace
cargo check -p launcher-ui --features launcher-ui/tauri-backend
```

Run the full app (WebView + Rust backend) from the repo root:

```sh
cargo tauri dev
```

Portable fallback (no WebView linked) plus static frontend preview:

```sh
cargo run
python3 -m http.server 1420 --directory crates/ui/frontend/dist
# open http://localhost:1420 (preview only, backend disconnected)
```

Optional at runtime (no rebuild needed):

```sh
PRISM_MSA_CLIENT_ID=<your-azure-client-id> cargo run
```

`PRISM_MSA_CLIENT_ID` supplies the Azure app registration id. The compiled-in
default comes from the same variable at **build** time
(`PRISM_MSA_CLIENT_ID=... cargo build`); without either, the build falls back
to a null placeholder and login fails loudly with a clear "no Azure client-ID
configured" message instead of a cryptic 403. The release workflow accepts
this variable as a secret holding a testing client ID.

Login flow (blocking login modal on boot):

1. Click "Se connecter avec Microsoft" (the code is auto-copied and the
   Microsoft page auto-opens).
2. Enter the `user_code` at `https://www.microsoft.com/link` and approve;
   the launcher polls the token endpoint, then chains XBL → XSTS →
   Minecraft automatically.

Play flow ("Accueil" screen):

1. Press "JOUER" directly — it opens the install-options modal when an
   install/update is pending (`Init`/`OutOfDate`; includes the optional Loki
   checkbox), installs, then launches; it opens the login modal when logged
   out.
2. "JOUER" spawns the Forge command detached; with live console on, the
   output streams into the Console tab, otherwise stdio is nulled. The game
   keeps running if you close the launcher.
3. "Actualiser le manifeste" re-fetches the manifest.

## Project layout

```text
src/main.rs                top-level `launcher` binary (portable fallback)
crates/core/               launcher-core: manifest, OS filter, integrity,
                           downloads, instance dirs, Java provisioning,
                           Forge 1.7.10 launch command, modern (LWJGL3) stack
crates/auth/               launcher-auth: Microsoft device-code + XBL/XSTS +
                           Minecraft Services + keyring helpers
crates/ui/                 launcher-ui: Tauri backend + static frontend
  src/state.rs             shared AppState, settings, DTOs, launch argv
  src/commands.rs          invoke backend as plain async fns (headless tests)
  src/tauri_app.rs         WebView wiring (feature `tauri-backend` only)
  src/server_status.rs     Java Server List Ping
  src/patch_notes.rs       CHANGELOG + installed version + GitHub releases
  frontend/dist/           index.html + styles.css + main.js (vanilla JS,
                           no npm) + vendored fonts
src-tauri/                 Tauri CLI harness (own package, workspace-excluded;
                           tauri.conf.json, capabilities, icons)
stitch_46anarchy_minecraft_launcher/  design mocks documenting UI intent
.github/workflows/        CI + release pipelines
```

Public core API (see `crates/core/src/lib.rs`): `default_game_dir`,
`fetch_manifest`, `plan_downloads_for_current_os`, `download_files`,
`compare_manifests`, `build_full_command`, and friends. Auth entry points
(`crates/auth/src/lib.rs`): `request_device_code`, `poll_device_token`,
`full_login_with_device_code`, `refresh_session`, keyring helpers
`save_refresh_token` / `load_refresh_token` / `clear_refresh_token`.

## CI and releases

- **CI** (`.github/workflows/ci.yml`): `fmt --check` + `clippy -D warnings`
  + `cargo test --workspace` on every push/PR to `main`/`master`.
  Installs `libdbus-1-dev` only (keyring link step); no WebKit needed
  because tests stay headless by design.
- **Release** (`.github/workflows/release.yml`): pushing a `v*` tag builds
  per-OS bundles via `tauri-apps/tauri-action@v0` (`ubuntu-22.04`:
  deb/appimage/rpm; `windows-latest`: nsis/msi; `macos-latest`: dmg) and
  attaches them to a **draft** GitHub Release for manual review and publish.
  Linux additionally builds an `x86-64-v3` variant
  (`RUSTFLAGS="-C target-cpu=x86-64-v3"`, post-build `-v3` rename) attached
  to the same draft; both share the untouched `[profile.release]`
  LTO/strip setup.
- **Secrets** (all optional): the release job reads them from a **`release`
  environment** (repo Settings → Environments), never as plain repository
  secrets: `PRISM_MSA_CLIENT_ID` (testing Azure id baked into release
  builds), `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]` (updater artifacts —
  enable `updater` in `tauri.conf.json` first; it is currently off),
  `APPLE_CERTIFICATE[_PASSWORD]`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`,
  `APPLE_PASSWORD`, `APPLE_TEAM_ID` for macOS signing/notarization.
  Windows code signing is not wired: extend the tauri-action step per
  <https://v2.tauri.app/distribute/sign/windows/>.
- Updater keypair for later: `cargo tauri signer generate`.
- The `src-tauri/icons/` set was generated from a placeholder diamond —
  replace with final artwork and re-run `cargo tauri icon` before a public
  release. No `LICENSE` file is committed yet; sources carry AGPL-3.0-only
  SPDX headers and this README references the license.

## Configuration and data locations

- Instance root: `~/.46anarchy/paladium` (Linux/macOS `$HOME`, Windows
  `%APPDATA%`; the trailing `paladium` segment is load-bearing for the mods).
  A `game_dir` settings override is normalized the same way. Contains
  `mods/` `libraries/` `natives/` `versions/` `assets/` `java/` (`java21/`
  on the experimental path), `logs/launcher-console.log`, `cdn_last.json`
  (last applied manifest), `.installed_flavor` (`default` | `experimental`),
  and the settings file below.
- Settings file: `<gameDir>/launcher-settings.json` (`memory_min`,
  `memory_max`, `java_path`, `game_dir`, `client_id`, `resolution`,
  `fullscreen`, `console_live`, `jvm_preset`, `jvm_args`,
  `experimental_runtime`, `experimental_optimizations`, account roster).
  Azure client-ID priority: settings `client_id` field → `PRISM_MSA_CLIENT_ID`
  env → build default.
- Keyring service: `fr.46anarchy.launcher` (MSA refresh tokens, one entry
  per account UUID). Fixed endpoints: manifest
  `https://cdn.46anarchy.fr/manifest.json`, server status
  `mc.46anarchy.fr:25566`.

## Troubleshooting

- **Blank window on NVIDIA / llvmpipe:** run with
  `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1`. It forces the WebKit renderer through
  shared-memory instead of DMA-BUF (display-only fallback, no RAM saving).
- Deliberately not recommended for RAM (one line each): `JavaScriptCoreUseJIT=0`
  kills the JS JIT the UI polls/renders on — slower wakeups for a negligible
  code-cache saving; `Malloc=1`-style allocator overrides impose no ceiling,
  only fragmentation risk; sandbox disables (e.g.
  `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1`) drop process isolation for zero
  RAM; `ulimit` memory caps crash the renderer instead of trimming usage —
  a crash, not a diet.

## License

`AGPL-3.0-only`. See <https://www.gnu.org/licenses/agpl-3.0.html>.

Links: [46anarchy.fr](https://46anarchy.fr) ·
[FS-Patches](https://github.com/46Anarchy/FS-Patches) ·
[discord.46anarchy.fr](https://discord.46anarchy.fr)
