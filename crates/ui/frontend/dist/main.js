/* SPDX-License-Identifier: AGPL-3.0-only */
/* 46Anarchy launcher — vanilla JS frontend, Obsidian Kinetic theme.
   No build step, no npm: every row calls a real Tauri `invoke` command.
   There is no demo/mock data anywhere in this file. */
(function () {
  "use strict";

  var SERVER_ADDRESS = "mc.46anarchy.fr";
  var DISCORD_URL = "https://discord.46anarchy.fr";
  var MS_LINK = "https://www.microsoft.com/link";
  var NO_TAURI_MSG = "Ouvrez dans l'app Tauri — backend indisponible.";
  var LOG_PLACEHOLDER = "Les détails d'installation apparaîtront ici.";
  var STATUS_INTERVAL_MS = 30000;
  // Secours "En jeu" par sondage (`get_game_status`, sans bibliothèque) :
  // la distribution des événements vers cette WebView n'est pas fiable
  // (progression/console/verdict tous non observés), l'appel `invoke`
  // fonctionne pour tout. L'écouteur `game-outcome` reste inchangé
  // (premier signal gagne, même drapeau).
  var GAME_STATUS_INTERVAL_MS = 3000;
  var TPS_UNAVAILABLE_TIP = "TPS non exposé par le ping";

  // Presets JVM : `args` (ligne injectée au lancement, fusionnée avec la
  // mémoire du slider — aucun preset ne contient -Xmx, voir `launch_argv`)
  // + `flavors` (modpacks autorisés : `default` = Paladium Java 8,
  // `experimental` = modernisé Java 21). Les presets GraalVM portent la base
  // ZGC expérimentale ; « basse RAM » = même base sans AlwaysPreTouch
  // (le pré-touch réserve tout le tas d'avance — l'équivalent portable du
  // delta low-RAM G1GC, sans tuning exotique).
  var JVM_PRESETS = {
    "46anarchy": { args: "-XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=200 -XX:+UnlockExperimentalVMOptions -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -Dfml.ignoreInvalidMinecraftCertificates=true -Dfml.ignorePatchDiscrepancies=true", flavors: ["default"] },
    "g1gc": { args: "-XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=150 -XX:+UnlockExperimentalVMOptions -XX:+AlwaysPreTouch -XX:G1ReservePercent=15 -Dfml.ignoreInvalidMinecraftCertificates=true", flavors: ["default"] },
    "lowram": { args: "-XX:+UseConcMarkSweepGC -XX:+CMSIncrementalMode -XX:-UseAdaptiveSizePolicy -Xmn512m -Dfml.ignoreInvalidMinecraftCertificates=true", flavors: ["default"] },
    "graal-zgc": { args: "-XX:+UseZGC -XX:+ZGenerational -XX:+AlwaysPreTouch -XX:+ParallelRefProcEnabled -XX:+DisableExplicitGC -Dfml.ignoreInvalidMinecraftCertificates=true -Dfml.ignorePatchDiscrepancies=true", flavors: ["experimental"] },
    "graal-lowram": { args: "-XX:+UseZGC -XX:+ZGenerational -XX:+ParallelRefProcEnabled -XX:+DisableExplicitGC -Dfml.ignoreInvalidMinecraftCertificates=true -Dfml.ignorePatchDiscrepancies=true", flavors: ["experimental"] }
  };

  function jvmPresetArgs(name) {
    var p = JVM_PRESETS[name];
    if (!p) return "";
    return (typeof p === "string") ? p : (p.args || "");
  }

  function $(id) { return document.getElementById(id); }
  function on(id, ev, fn) {
    var el = $(id);
    if (el) el.addEventListener(ev, fn);
    return el;
  }

  var toastTimer = null;
  function toast(msg) {
    var el = $("toast");
    if (!el) return;
    var txt = $("toast-text");
    if (txt) txt.textContent = msg;
    else el.textContent = msg;
    el.classList.remove("hidden");
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { el.classList.add("hidden"); }, 4000);
  }

  // Suivi d'épingle : collage en bas à l'ajout IFF (nouvelle ligne ajoutée
  // ET case cochée ET lecteur à ~24px du bas AVANT l'ajout). Jamais de
  // collage à l'affichage de l'onglet, jamais quand le journal est caché
  // (on ajoute seulement), jamais de lecteur remonté arraché. Couvre
  // `#install-log` et `#game-log` (mêmes aides).
  var NEAR_BOTTOM_PX = 24;
  // `#install-log` : même plafond que `#game-log` (500 lignes, même découpe
  // par l'avant — le journal d'installation était le seul conteneur DOM à
  // croissance illimitée).
  var INSTALL_LOG_CAP = 500;
  var installLogCount = 0;
  function autoscrollOn() {
    var box = $("console-autoscroll");
    return !box || box.checked;
  }
  function isNearBottom(el) {
    if (!el) return false;
    // Caché (onglet invisible) : aucune mesure fiable — traité comme NON
    // ancré : on ajoute la ligne sans toucher au défilement.
    if (el.clientHeight === 0) return false;
    return (el.scrollHeight - el.scrollTop - el.clientHeight) <= NEAR_BOTTOM_PX;
  }
  function scrollLogBottom(el) {
    if (!el) return;
    try { el.scrollTop = el.scrollHeight; } catch (e) {}
  }

  // Rafales : les lignes sont mises en file puis vidées en un seul
  // `DocumentFragment` par trame (une lecture de mise en page, un collage),
  // ordre préservé, plafond et pin-follow inchangés.
  var installLogQueue = [];
  var installLogScheduled = false;
  function scheduleInstallFlush() {
    if (installLogScheduled) return;
    installLogScheduled = true;
    function run() { installLogScheduled = false; flushInstallLog(); }
    if (typeof requestAnimationFrame === "function") requestAnimationFrame(run);
    else setTimeout(run, 32);
  }
  function flushInstallLog() {
    if (!installLogQueue.length) return;
    var el = $("install-log");
    if (!el) { installLogQueue.length = 0; installLogCount = 0; return; }
    var pinned = autoscrollOn() && isNearBottom(el);
    if (el.textContent === LOG_PLACEHOLDER || el.textContent === "—") {
      el.textContent = "";
      installLogCount = 0;
    }
    var frag = document.createDocumentFragment();
    for (var i = 0; i < installLogQueue.length; i++) {
      frag.appendChild(document.createTextNode(installLogQueue[i] + "\n"));
    }
    installLogQueue.length = 0;
    el.appendChild(frag);
    installLogCount += i;
    while (installLogCount > INSTALL_LOG_CAP && el.firstChild) {
      el.removeChild(el.firstChild);
      installLogCount -= 1;
    }
    if (pinned) scrollLogBottom(el);
  }
  function log(line) {
    installLogQueue.push(String(line));
    scheduleInstallFlush();
  }

  function copyText(text, okMsg, silent) {
    function done() { if (!silent) toast(okMsg || "Copié."); }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { if (!silent) toast("Copie impossible."); });
    } else {
      var ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand("copy"); done(); } catch (e) { if (!silent) toast("Copie impossible."); }
      document.body.removeChild(ta);
    }
  }

  /* External links always open in the OS browser via the backend
     (`open_external`: allowlisted https hosts, detached xdg-open/open).
     No in-app WebView navigation, no opener plugin needed. */
  function openUrl(href) {
    if (HAS_TAURI) {
      invoke("open_external", { url: href }).then(function () {}, function (err) {
        toast(typeof err === "string" ? err : "Ouverture impossible.");
      });
    } else {
      window.open(href, "_blank", "noopener");
    }
  }

  /* Live game console (console-line events when `console_live` is on).
     Capped at 500 rows so a chatty mod can't freeze the WebView. */
  var GAME_LOG_CAP = 500;
  var gameLogCount = 0;
  var gameLogQueue = [];
  var gameLogScheduled = false;
  function scheduleGameFlush() {
    if (gameLogScheduled) return;
    gameLogScheduled = true;
    function run() { gameLogScheduled = false; flushGameLog(); }
    if (typeof requestAnimationFrame === "function") requestAnimationFrame(run);
    else setTimeout(run, 32);
  }
  function flushGameLog() {
    if (!gameLogQueue.length) return;
    var el = $("game-log");
    if (!el) { gameLogQueue.length = 0; gameLogCount = 0; return; }
    var pinned = autoscrollOn() && isNearBottom(el);
    if (el.dataset.placeholder === "1") {
      el.textContent = "";
      el.dataset.placeholder = "";
      gameLogCount = 0;
    }
    var frag = document.createDocumentFragment();
    for (var i = 0; i < gameLogQueue.length; i++) {
      var div = document.createElement("div");
      div.textContent = gameLogQueue[i];
      frag.appendChild(div);
    }
    gameLogQueue.length = 0;
    el.appendChild(frag);
    gameLogCount += i;
    while (gameLogCount > GAME_LOG_CAP && el.firstChild) {
      el.removeChild(el.firstChild);
      gameLogCount -= 1;
    }
    if (pinned) scrollLogBottom(el);
  }
  function glog(line) {
    gameLogQueue.push(String(line));
    scheduleGameFlush();
  }

  function clearGameLog() {
    gameLogQueue.length = 0;
    var el = $("game-log");
    if (!el) return;
    el.textContent = "";
    el.dataset.placeholder = "";
    gameLogCount = 0;
  }

  function formatBytes(bytes) {
    bytes = Number(bytes) || 0;
    if (bytes < 1024) return bytes + " o";
    if (bytes < 1048576) return (bytes / 1024).toFixed(1) + " Kio";
    if (bytes < 1073741824) return (bytes / 1048576).toFixed(1) + " Mo";
    return (bytes / 1073741824).toFixed(2) + " Gio";
  }

  /* ---------- backend bridge (real Tauri invoke only, no fallback data) ---------- */

  var tauriCore = window.__TAURI__ && window.__TAURI__.core ? window.__TAURI__.core : null;
  var tauriEvent = window.__TAURI__ && window.__TAURI__.event ? window.__TAURI__.event : null;
  var HAS_TAURI = !!(tauriCore && tauriCore.invoke);

  function invoke(cmd, args) {
    if (HAS_TAURI) return tauriCore.invoke(cmd, args || {});
    return Promise.reject(NO_TAURI_MSG);
  }

  function requireBackend() {
    if (!HAS_TAURI) toast(NO_TAURI_MSG);
    return HAS_TAURI;
  }

  function isLoginRequired(err) {
    return typeof err === "string" && err.indexOf("LOGIN_REQUIRED") === 0;
  }

  function disableBackendActions() {
    var ids = ["btn-play", "btn-install", "btn-cancel-install", "btn-verify", "btn-refresh-manifest",
      "pack-flavor-btn", "pack-flavor-edit", "res-btn",
      "btn-open-mods", "btn-open-folder",
      "btn-microsoft", "btn-login-accounts", "btn-loki-install"];
    ids.forEach(function (id) {
      var el = $(id);
      if (el) el.disabled = true;
    });
  }

  function showNoBackendMode() {
    var banner = $("demo-banner");
    if (banner) banner.classList.remove("hidden");
    disableBackendActions();
    var sl = $("status-line");
    if (sl) sl.textContent = NO_TAURI_MSG;
    var sub = $("client-sub");
    if (sub) sub.textContent = "—";
    var msub = $("mods-subtitle");
    if (msub) msub.textContent = NO_TAURI_MSG;
    var list = $("mods-list");
    if (list) {
      list.innerHTML = "";
      var empty = document.createElement("div");
      empty.className = "mod-row";
      var inner = document.createElement("div");
      inner.className = "mod-main";
      var span = document.createElement("span");
      span.className = "muted";
      span.style.fontSize = "12px";
      span.textContent = NO_TAURI_MSG;
      inner.appendChild(span);
      empty.appendChild(inner);
      list.appendChild(empty);
    }
    var sv = $("stat-version"), ss = $("stat-status"), sp = $("stat-player");
    if (sv) sv.textContent = "—";
    if (ss) ss.textContent = "—";
    if (sp) sp.textContent = "—";
    var det = $("install-detail");
    if (det) det.textContent = NO_TAURI_MSG;
    var sp2 = $("server-ping"), sol = $("server-online-label");
    if (sp2) sp2.textContent = "—";
    if (sol) sol.textContent = NO_TAURI_MSG;
    var tps = $("server-tps");
    if (tps) {
      tps.textContent = "—";
      tps.title = TPS_UNAVAILABLE_TIP;
    }
    var box = $("patch-notes");
    if (box) {
      box.innerHTML = "";
      box.appendChild(patchMessageCard("Indisponible", NO_TAURI_MSG));
    }
  }

  /* ---------- tabs ---------- */

  // Liste statique mise en cache une fois (le changement d'onglet ne
  // rescane plus le DOM à chaque bascule).
  var navItemsCache = null;
  var tabPagesCache = null;
  function getNavItems() {
    if (!navItemsCache) navItemsCache = document.querySelectorAll(".nav-item");
    return navItemsCache;
  }
  function getTabPages() {
    if (!tabPagesCache) {
      tabPagesCache = {
        home: $("tab-home"), mods: $("tab-mods"), java: $("tab-java"),
        accounts: $("tab-accounts"), console: $("tab-console")
      };
    }
    return tabPagesCache;
  }

  function switchTab(name) {
    var items = getNavItems();
    for (var i = 0; i < items.length; i++) {
      items[i].classList.toggle("active", items[i].getAttribute("data-tab") === name);
    }
    var pages = getTabPages();
    for (var key in pages) {
      var page = pages[key];
      if (page) page.classList.toggle("hidden", key !== name);
    }
    if (name === "mods" && HAS_TAURI) refreshMods();
    if (name === "home" && HAS_TAURI) {
      fetchServerStatus();
      fetchPatchNotes();
    }
  }

  /* ---------- login-first modal ---------- */

  var countdownTimer = null;
  var sessionCached = null;
  // Boot auth gate (poll-until-settled, no time-based modal): the backend
  // restores the keyring session asynchronously (instant on raw binary,
  // seconds late on AppImage: cold WebKit/squashfs/keyring-unlock), so an
  // early null must NEVER pop the login modal — it would flash then vanish.
  // `get_session` is polled every 1s up to 15s instead: the first non-null
  // session takes the logged-in path (the modal never opens) and the
  // `session-changed` event settles immediately; the modal opens ONLY on
  // sustained null past 15s. While waiting, the home shows a subtle
  // non-modal hint (`Vérification de la session…`), never a modal. A
  // genuinely logged-out user waits ≤15s with that honest hint.
  var BOOT_POLL_MS = 1000;
  var BOOT_POLL_MAX = 15;
  var bootAuthResolved = false;
  var bootAuthPending = true;
  var bootPollTimer = null;
  var bootPollCount = 0;
  // Génération de connexion : chaque `startLogin` et chaque `cancelLogin`
  // l'incrémente ; les règlements tardifs de `poll_login` (promesse non
  // annulable, ~15 min) dont la génération ne correspond plus sont IGNORÉS
  // (ni toast, ni session). `bootLoginDismissed` tient l'armistice avec la
  // barrière de démarrage : annuler une PREMIÈRE connexion ne doit pas voir
  // la modale rouvrir ~1s plus tard sur le sustained-null ; les entrées
  // manuelles (`startLogin`) l'effacent (l'utilisateur a changé d'avis).
  var loginGen = 0;
  var bootLoginDismissed = false;
  var lastStatus = null;
  var playBusy = false;
  var installBusy = false;
  // Suivi "En jeu" : l'événement `game-outcome` reste le premier signal,
  // le sondage `get_game_status` (3s) + `get_last_outcome` synchronise
  // sans lui. Caveat connu : si le launcher redémarre pendant que le jeu
  // tourne, le PID est inconnu et le bouton paraît normal — acceptable.
  var gameRunning = false;
  var gamePid = null;

  function openLoginModal() {
    var m = $("login-modal");
    if (m) m.classList.remove("hidden");
  }
  function closeLoginModal() {
    var m = $("login-modal");
    if (m) m.classList.add("hidden");
  }

  function showDevicePanel(info) {
    var init = $("login-initial"), panel = $("device-panel");
    if (init) init.classList.add("hidden");
    if (panel) panel.classList.remove("hidden");
    var derr = $("device-error");
    if (derr) derr.classList.add("hidden");
    var retry = $("btn-retry-login");
    if (retry) retry.classList.add("hidden");
    var uri = info.verification_uri_complete || info.verification_uri || MS_LINK;
    var link = $("verification-link");
    if (link) {
      link.href = uri;
      link.textContent = info.verification_uri || MS_LINK;
    }
    var code = $("user-code");
    if (code) code.textContent = info.user_code;
    if (info.user_code) copyText(info.user_code, null, true);
    openUrl(uri);
    var remaining = info.expires_in || 900;
    if (countdownTimer) clearInterval(countdownTimer);
    var cdCached = $("device-countdown");
    function tick() {
      var cd = cdCached || $("device-countdown");
      if (cd && !cdCached) cdCached = cd;
      var m = Math.floor(remaining / 60), s = remaining % 60;
      var label = (m < 10 ? "0" + m : m) + ":" + (s < 10 ? "0" + s : s);
      if (cd) cd.textContent = label;
      if (remaining <= 0) {
        if (countdownTimer) clearInterval(countdownTimer);
        if (cd) cd.textContent = "expiré — relancez la connexion";
        return;
      }
      remaining -= 1;
    }
    tick();
    countdownTimer = setInterval(tick, 1000);
  }

  function resetLoginModal() {
    if (countdownTimer) clearInterval(countdownTimer);
    var init = $("login-initial"), panel = $("device-panel");
    if (init) init.classList.remove("hidden");
    if (panel) panel.classList.add("hidden");
    var err = $("login-error");
    if (err) err.classList.add("hidden");
  }

  function startLogin() {
    if (!requireBackend()) return;
    // Entrée manuelle (bouton Microsoft, Comptes, nouvel essai) :
    // l'utilisateur a changé d'avis, fin de l'armistice de démarrage.
    bootLoginDismissed = false;
    loginGen += 1;
    var myGen = loginGen;
    var hadSession = !!sessionCached;
    openLoginModal();
    var err0 = $("login-error");
    if (err0) err0.classList.add("hidden");
    var btn = $("btn-microsoft");
    if (btn) btn.disabled = true;
    invoke("request_login").then(function (info) {
      if (myGen !== loginGen) return;
      if (btn) btn.disabled = false;
      showDevicePanel(info);
      invoke("poll_login").then(function (session) {
        if (myGen !== loginGen) return;
        if (countdownTimer) clearInterval(countdownTimer);
        resetLoginModal();
        closeLoginModal();
        renderSession(session);
        refreshAccounts();
        toast(hadSession ? "Compte ajouté : " + session.name + "." : "Connecté : " + session.name + ".");
      }, function (err) {
        if (myGen !== loginGen) return;
        if (countdownTimer) clearInterval(countdownTimer);
        var el = $("device-error");
        if (el) {
          el.textContent = typeof err === "string" ? err : "Connexion refusée.";
          el.classList.remove("hidden");
        }
        var retry = $("btn-retry-login");
        if (retry) retry.classList.remove("hidden");
      });
    }, function (err) {
      if (myGen !== loginGen) return;
      if (btn) btn.disabled = false;
      if (isLoginRequired(err)) { openLoginModal(); return; }
      var el = $("login-error");
      if (el) {
        el.textContent = typeof err === "string" ? err : "Erreur de connexion.";
        el.classList.remove("hidden");
      }
    });
  }

  // Annulation : invalide les règlements `poll_login` en vol (garde de
  // génération, sans toast ni session), efface le `pending_login` backend
  // (pare-feu + garde, approbation tardive au téléphone sans effet),
  // referme la modale. Connecté, ce n'est qu'une fermeture ; primo-connexion,
  // l'armistice `bootLoginDismissed` tient la barrière de démarrage.
  function cancelLogin() {
    loginGen += 1;
    if (countdownTimer) clearInterval(countdownTimer);
    var btn = $("btn-microsoft");
    if (btn) btn.disabled = false;
    invoke("cancel_login").then(function () {}, function () {});
    resetLoginModal();
    closeLoginModal();
    bootLoginDismissed = true;
  }

  /* ---------- rendering (real backend fields only) ---------- */

  // Avatars go through the backend (`get_avatar`: disk-cached data URL,
  // never a remote <img>). A missing or failed avatar leaves the glyph
  // backdrop visible (no src attribute), never an empty square.
  // Cache session des avatars par uuid : un seul `invoke` par uuid,
  // jamais de `src` identique réécrit (les listes de comptes reconstruisent
  // deux lignes par compte à chaque `refreshAccounts`).
  var avatarUrlCache = {};
  var avatarPending = {};
  function loadAvatar(img, uuid) {
    if (!img) return;
    var u = String(uuid || "").replace(/-/g, "");
    if (!/^[0-9a-f]{32}$/i.test(u)) {
      img.removeAttribute("src");
      try { delete img.dataset.avatarUuid; } catch (e) {}
      return;
    }
    var key = u.toLowerCase();
    if (img.dataset.avatarUuid === key && img.hasAttribute("src")) return;
    img.dataset.avatarUuid = key;
    if (avatarUrlCache[key]) {
      img.src = avatarUrlCache[key];
      return;
    }
    if (avatarPending[key]) {
      avatarPending[key].push(img);
      return;
    }
    avatarPending[key] = [img];
    invoke("get_avatar", { uuid: uuid }).then(function (url) {
      avatarUrlCache[key] = url;
      var waiters = avatarPending[key] || [];
      delete avatarPending[key];
      for (var i = 0; i < waiters.length; i++) {
        if (waiters[i].dataset.avatarUuid === key) waiters[i].src = url;
      }
    }, function () {
      var waiters2 = avatarPending[key] || [];
      delete avatarPending[key];
      for (var j = 0; j < waiters2.length; j++) {
        if (waiters2[j].dataset.avatarUuid === key) waiters2[j].removeAttribute("src");
      }
    });
  }

  function setAvatar(id, uuid) {
    loadAvatar(document.getElementById(id), uuid);
  }

  // Glyph-backdrop avatar box for account rows and menu items (the <img>
  // overlays the person glyph once the backend data URL lands).
  function avatarMini(uuid, small) {
    var wrap = document.createElement("span");
    wrap.className = "avatar-mini" + (small ? " sm" : "");
    var svgNS = "http://www.w3.org/2000/svg";
    var svg = document.createElementNS(svgNS, "svg");
    svg.setAttribute("class", "icon");
    var use = document.createElementNS(svgNS, "use");
    use.setAttribute("href", "#i-person");
    svg.appendChild(use);
    wrap.appendChild(svg);
    var img = document.createElement("img");
    img.className = "avatar-img";
    img.alt = "";
    wrap.appendChild(img);
    loadAvatar(img, uuid);
    return wrap;
  }

  function settleBootAuth() {
    bootAuthPending = false;
    if (bootPollTimer) { clearInterval(bootPollTimer); bootPollTimer = null; }
  }

  function setBootHint() {
    var sl = $("status-line");
    if (sl && !lastStatus) sl.textContent = "Vérification de la session…";
    var sp = $("stat-player");
    if (sp && !sessionCached) sp.textContent = "Vérification…";
  }

  // Poll-until-settled: every 1s up to 15s, the first non-null session wins
  // (logged-in path, the modal never opens); sustained null past 15s opens
  // the modal once. `session-changed` settles immediately (see listener).
  function startBootAuthPoll() {
    if (!HAS_TAURI) { settleBootAuth(); return; }
    setBootHint();
    if (bootPollTimer) return;
    bootPollTimer = setInterval(function () {
      if (!bootAuthPending) { settleBootAuth(); return; }
      if (sessionCached || !HAS_TAURI) { settleBootAuth(); return; }
      bootPollCount += 1;
      if (bootPollCount >= BOOT_POLL_MAX) {
          settleBootAuth();
          bootAuthResolved = true;
          if (!sessionCached && HAS_TAURI) {
            invoke("get_session").then(function (s) {
              if (s) { applySessionUpdate(s); return; }
              var sp = $("stat-player");
              if (sp) sp.textContent = "—";
              setAvatar("avatar-top-img", null);
              resetLoginModal();
              // Armistice : une primo-connexion annulée ne rouvre pas.
              if (!bootLoginDismissed) openLoginModal();
            }, function () {
              var sp2 = $("stat-player");
              if (sp2) sp2.textContent = "—";
              resetLoginModal();
              if (!bootLoginDismissed) openLoginModal();
            });
          }
        return;
      }
      invoke("get_session").then(function (s) {
        if (!bootAuthPending) return;
        if (s) { applySessionUpdate(s); }
        else { setBootHint(); }
      }, function () {});
    }, BOOT_POLL_MS);
  }

  function renderSession(session, force) {
    sessionCached = session || null;
    var chip = $("user-chip");
    if (session) {
      settleBootAuth();
      bootAuthResolved = true;
      // Une vraie session rend l'armistice sans objet (la barrière ne tire
      // que sur sustained-null) : la prochaine déconnexion rouvrira la modale.
      bootLoginDismissed = false;
      if (chip) chip.classList.remove("hidden");
      var nm = $("user-name");
      if (nm) nm.textContent = session.name;
      var sp = $("stat-player");
      if (sp) sp.textContent = session.name;
      setAvatar("avatar-top-img", session.uuid);
      closeLoginModal();
    } else {
      // Stale null guard: a slow boot poll resolving AFTER a restore must
      // not clobber the live session (it left the modal stuck open over a
      // restored account). Explicit paths (logout) pass force=true.
      if (sessionCached && !force) {
        return;
      }
      if (chip) chip.classList.add("hidden");
      if (bootAuthPending && !force) {
        // Restore may still land: subtle non-modal hint only, never a modal.
        setBootHint();
        return;
      }
      var sp2 = $("stat-player");
      if (sp2) sp2.textContent = "—";
      setAvatar("avatar-top-img", null);
      resetLoginModal();
      // Garde périmée + armistice : jamais de modale sur sustained-null
      // après une annulation (sauf chemin explicite `force`).
      if (!bootLoginDismissed || force) openLoginModal();
    }
  }

  var FRIENDLY_STATUS = {
    init: "Jeu non installé — cliquez JOUER",
    ok: "À jour — prêt à jouer",
    out_of_date: "Mise à jour requise — cliquez JOUER",
    cdn_failure: "Impossible de vérifier la version — réessayez plus tard"
  };

  // Home mod-count line: derived from the real `get_mods_list` cache +
  // the real `get_manifest_status` detail. Never hardcoded.
  function updateHomeModLine() {
    if (!lastStatus) return;
    var sub = $("client-sub");
    if (!sub) return;
    var base = lastStatus.detail || "";
    var n = modsCache.length;
    if (n > 0) {
      sub.textContent = n + (n > 1 ? " mods • " : " mod • ") + base;
    } else {
      sub.textContent = base || "—";
    }
  }

  // `get_manifest_status` -> version pill, statusbar, status row, maintenance.
  // The bottom bar no longer shows the modpack version (it stays in
  // `#version-pill`); `#stat-version` is the launcher version (see
  // `fetchLauncherVersion`).
  function renderStatus(st) {
    lastStatus = st || {};
    var pill = $("version-pill");
    if (pill) {
      pill.innerHTML = "";
      var dot = document.createElement("span");
      dot.className = "dot";
      pill.appendChild(dot);
    var name = "46Anarchy";
    var mc = st.mc_version || "—";
    pill.appendChild(document.createTextNode(name + " • Forge " + mc));
    }
    var ss = $("stat-status");
    var label = { init: "—", ok: "À jour", out_of_date: "MàJ dispo", cdn_failure: "CDN injoignable" };
    if (ss) ss.textContent = label[st.update_status] || st.update_status || "—";
    var sl = $("status-line");
    if (sl) sl.textContent = FRIENDLY_STATUS[st.update_status] || st.detail || "—";
    updateHomeModLine();
    var cdot = $("client-dot");
    if (cdot) cdot.className = st.maintenance_enabled ? "dot warn" : (st.update_status === "ok" ? "dot" : "dot warn");
    var banner = $("maintenance-banner");
    if (banner) {
      if (st.maintenance_enabled) {
        banner.textContent = st.maintenance_message || "Maintenance en cours. Le jeu est temporairement indisponible.";
        banner.classList.remove("hidden");
      } else {
        banner.classList.add("hidden");
      }
    }
    refreshPlayButton();
    var vb = $("btn-verify");
    if (vb) {
      if (st.update_status === "init") {
        vb.disabled = true;
        vb.title = "Disponible une fois le jeu installé.";
      } else {
        vb.disabled = false;
        vb.title = "Recontrôler les fichiers locaux (aucun téléchargement).";
      }
    }
  }

  /* ---------- live server status (`get_server_status`) ---------- */

  // Server mini-card (#server-ping, #server-tps, #server-online-tag/label).
  // TPS is never exposed by the Server List Ping: #server-tps always shows
  // "—" with an explanatory tooltip, never a fake 20.0. The status label is
  // exactly "En ligne" / "Hors ligne" (never a long sentence that reflows
  // the card — full detail lives in the `title` tooltip only, the label is
  // truncated with ellipsis in CSS). Ping/TPS numbers are untouched.
  // Change-detection: the 30s poll skips identical states (stable fields by
  // signature, ping only when its text changed).
  var lastServerStableSig = null;
  var lastServerPingText = null;
  function serverStableSig(s) {
    return [s.online ? "1" : "0", s.version || "", s.players_online, s.players_max, s.motd || "", s.error || "", s.address || ""].join("|");
  }
  function renderServerStatus(s) {
    var tps = $("server-tps");
    if (tps) {
      tps.textContent = "—";
      tps.title = TPS_UNAVAILABLE_TIP;
    }
    var serverPing = $("server-ping"), tag = $("server-online-tag"), label = $("server-online-label");
    if (!s) return;
    var pingText = (s.online && s.ping_ms != null) ? (s.ping_ms + " ms") : "—";
    if (pingText !== lastServerPingText) {
      lastServerPingText = pingText;
      if (serverPing) serverPing.textContent = pingText;
    }
    var sig = serverStableSig(s);
    if (sig === lastServerStableSig) return;
    lastServerStableSig = sig;
    if (s.online) {
      if (label) label.textContent = "En ligne";
      if (tag) {
        tag.style.color = "";
        var bits = [];
        if (s.version) bits.push(s.version);
        if (s.players_online != null && s.players_max != null) bits.push(s.players_online + "/" + s.players_max + " joueurs");
        if (s.motd) bits.push(s.motd);
        if (s.address) bits.push(s.address);
        tag.title = bits.join(" — ");
        var d = tag.querySelector(".dot");
        if (d) d.className = "dot";
      }
    } else {
      if (label) label.textContent = "Hors ligne";
      if (tag) {
        tag.title = (typeof s.error === "string" && s.error) ? s.error : (s.address || "");
        var d2 = tag.querySelector(".dot");
        if (d2) d2.className = "dot off";
      }
    }
  }

  function renderServerStatusError(err) {
    renderServerStatus({
      online: false,
      address: SERVER_ADDRESS,
      error: typeof err === "string" ? err : "Serveur injoignable."
    });
  }

  // Fraîcheur anti-gaspillage : les revisites d'onglet et le retour de
  // fenêtre refont la même demande alors que le sondage 30s vient de passer.
  var lastServerFetchAt = 0;
  var SERVER_FRESH_MS = 20000;
  function fetchServerStatus(force) {
    if (!HAS_TAURI) return;
    // Diète RAM : aucun sondage planifié en fenêtre cachée (`force` = boot,
    // retour visible, onglet Accueil : passe toujours).
    if (!force && document.hidden) return;
    var now = Date.now();
    if (!force && (now - lastServerFetchAt) < SERVER_FRESH_MS) return;
    lastServerFetchAt = now;
    invoke("get_server_status").then(renderServerStatus, renderServerStatusError);
  }

  /* ---------- launcher version (`get_launcher_version`) ---------- */

  // Bottom bar: launcher version only (the modpack version stays in
  // `#version-pill`). On invoke failure the placeholder stays.
  function fetchLauncherVersion() {
    if (!HAS_TAURI) return;
    invoke("get_launcher_version").then(function (v) {
      var sv = $("stat-version");
      if (!sv || !v) return;
      sv.textContent = v.display || "—";
      if (v.detail) sv.title = v.detail;
    }, function () {});
  }

  // Tampon d'identité du build : une seule ligne au journal, échec silencieux.
  function fetchBuildInfo() {
    if (!HAS_TAURI) return;
    invoke("get_build_info").then(function (b) {
      if (!b) return;
      log("Build interface : " + (b.frontend_hash || "?") + " / moteur : " + (b.backend_hash || "?") + ".");
    }, function () {});
  }

  /* ---------- patch notes (`get_patch_notes`) ---------- */

  var KIND_LABELS = { major: "Majeur", hotfix: "Hotfix", info: "Info" };

  // Carte compacte (maquette : badge + repo/date à droite, titre, lien
  // court). Le corps brut (URL de commit de ~60 caractères) ne touche
  // jamais le DOM : seule la première URL est extraite et rendue comme lien
  // court `abcdef1 →` (SHA 7 caractères vers le commit) ; sans URL, un
  // extrait d'une ligne. Jamais d'URL brute.
  function patchCommitUrl(n) {
    var body = (n && n.body) || "";
    var m = body.match(/https?:\/\/\S+/);
    return m ? m[0].replace(/[),.\]]+$/, "") : "";
  }

  function patchShortSha(url) {
    var seg = (url.split("?")[0].split("#")[0].split("/").pop() || "").replace(/[^0-9a-f]/gi, "");
    return seg.slice(0, 7);
  }

  function patchCard(n) {
    var card = document.createElement("div");
    card.className = "card patch-card patch-compact";
    var top = document.createElement("div");
    top.className = "patch-top";
    var badge = document.createElement("span");
    var kind = (n && n.kind) || "info";
    badge.className = "patch-badge" + (kind === "info" ? " dim" : "");
    badge.textContent = KIND_LABELS[kind] || kind;
    top.appendChild(badge);
    var ver = document.createElement("span");
    ver.className = "ver";
    var verText = (n && n.version) || "";
    if (n && n.date) verText += (verText ? " • " : "") + n.date;
    ver.textContent = verText || "—";
    ver.title = verText || "";
    top.appendChild(ver);
    card.appendChild(top);
    var title = document.createElement("h3");
    title.textContent = (n && n.title) || "Sans titre";
    title.title = (n && n.title) || "";
    card.appendChild(title);
    var url = patchCommitUrl(n);
    if (url) {
      var sha = patchShortSha(url);
      var link = document.createElement("a");
      link.className = "patch-link";
      link.href = url;
      link.textContent = (sha || "commit") + " →";
      link.title = url;
      link.addEventListener("click", function (ev) {
        ev.preventDefault();
        openUrl(url);
      });
      card.appendChild(link);
    } else {
      var snippet = document.createElement("p");
      snippet.className = "patch-snippet";
      snippet.textContent = ((n && n.body) || "").split("\n")[0] || "—";
      card.appendChild(snippet);
    }
    return card;
  }

  function patchMessageCard(title, body) {
    var card = document.createElement("div");
    card.className = "card patch-card";
    var h = document.createElement("h3");
    h.textContent = title;
    card.appendChild(h);
    var p = document.createElement("p");
    p.textContent = body;
    card.appendChild(p);
    return card;
  }

  // Les notes sont re-demandées à chaque affichage de l'accueil : on ne
  // reconstruit le DOM que si le contenu a vraiment changé. L'accueil
  // montre 3 cartes compactes ; « Historique complet » déplie les 5 (même
  // traitement lien court, jamais d'URL brute).
  // Indice d'échec total du fetch distant : ligne atténuée sous le titre
  // (jamais de toast) ; le succès efface, l'échec affiche, le chargement
  // garde l'état précédent (pas de scintillement).
  var PATCH_UNREACHABLE_HINT = "Notes injoignables — vérifiez la connexion.";

  function renderPatchNotesSubtitle(unreachable) {
    var sub = $("patch-notes-subtitle");
    if (!sub) return;
    sub.textContent = unreachable ? PATCH_UNREACHABLE_HINT : "";
  }

  var lastPatchNotesSig = null;
  var lastPatchNotesCache = [];
  var lastPatchNotesUnreachable = false;
  var patchExpanded = false;
  // Signature légère sans `JSON.stringify` du corps complet : les champs
  // rendus + longueur du corps + URL courte suffisent à détecter un contenu
  // identique (les corps lointains ne sont jamais affichés).
  function patchNotesSig(notes, expanded, unreachable) {
    var parts = [(expanded ? "x" : "c"), (unreachable ? "u1" : "u0"), "n" + notes.length];
    for (var i = 0; i < notes.length; i++) {
      var n = notes[i] || {};
      var body = n.body || "";
      parts.push([n.version || "", n.kind || "", n.date || "", n.title || "", body.length, patchCommitUrl(n)].join("~"));
    }
    return parts.join("|");
  }
  function renderPatchNotes(feed) {
    var notes = Array.isArray(feed) ? feed : ((feed && feed.notes) || []);
    var unreachable = Array.isArray(feed) ? lastPatchNotesUnreachable : !!(feed && feed.unreachable);
    lastPatchNotesUnreachable = unreachable;
    lastPatchNotesCache = notes || [];
    var want = patchExpanded ? 5 : 3;
    var sig = patchNotesSig(lastPatchNotesCache, patchExpanded, unreachable);
    if (sig === lastPatchNotesSig) return;
    lastPatchNotesSig = sig;
    renderPatchNotesSubtitle(unreachable);
    var box = $("patch-notes");
    if (!box) return;
    box.innerHTML = "";
    var list = lastPatchNotesCache;
    if (list.length === 0) {
      box.appendChild(patchMessageCard("Aucune note", "Les notes de patch seront affichées ici dès qu'elles seront disponibles."));
      return;
    }
    for (var i = 0; i < Math.min(list.length, want); i++) {
      box.appendChild(patchCard(list[i]));
    }
  }

  // Boîte « vide » = rien, ou seulement la carte `Chargement…` (carte
  // statique du premier affichage ou d'un fetch en cours, marquée
  // `patch-loading`) : les cartes rendues, la carte d'erreur et la carte
  // « Aucune note » sont du contenu à garder, jamais à écraser.
  function patchBoxIsEmpty(box) {
    if (!box) return true;
    var kids = box.children;
    if (kids.length === 0) return true;
    for (var i = 0; i < kids.length; i++) {
      if (!kids[i].classList.contains("patch-loading")) return false;
    }
    return true;
  }

  var lastPatchFetchAt = 0;
  var PATCH_FRESH_MS = 60000;
  function fetchPatchNotes(force) {
    // Diète RAM : aucun refetch planifié en fenêtre cachée (`force` = boot,
    // retour visible, onglet Accueil : passe toujours) ; le retour visible
    // refetche déjà via `visibilitychange`.
    if (!force && document.hidden) return;
    var box = $("patch-notes");
    if (!force && (Date.now() - lastPatchFetchAt) < PATCH_FRESH_MS && !patchBoxIsEmpty(box)) return;
    if (!HAS_TAURI) {
      if (box) {
        box.innerHTML = "";
        box.appendChild(patchMessageCard("Indisponible", NO_TAURI_MSG));
      }
      return;
    }
    // Ne jamais détruire un contenu affiché pour un fetch incertain : la
    // carte `Chargement…` n'est dessinée que sur une boîte vide. La garde
    // de signature de `renderPatchNotes` ignore les flux identiques —
    // écraser les cartes avant un flux identique (le cas du cache au retour
    // sur l'accueil) laissait `Chargement…` pour toujours.
    if (box && patchBoxIsEmpty(box)) {
      box.innerHTML = "";
      var loading = patchMessageCard("Chargement…", "Récupération des notes de patch…");
      loading.classList.add("patch-loading");
      box.appendChild(loading);
    }
    lastPatchFetchAt = Date.now();
    invoke("get_patch_notes").then(renderPatchNotes, function (err) {
      renderPatchNotesSubtitle(true);
      var box2 = $("patch-notes");
      if (!box2) return;
      // Échec : garder le contenu périmé affiché ; la carte d'erreur ne
      // remplace qu'une boîte vide (dont la carte `Chargement…`).
      if (!patchBoxIsEmpty(box2)) return;
      box2.innerHTML = "";
      box2.appendChild(patchMessageCard(
        "Notes injoignables",
        typeof err === "string" ? err : "Impossible de charger les notes de patch."
      ));
    });
  }

  // `label` optionnel : les flux d'installation gardent "Mise à jour…",
  // le lancement (`doPlay`) passe "Lancement…". Sans `pct` (lancement),
  // aucun pourcentage n'est suffixé.
  function setPlayBusy(busy, pct, label) {
    var btn = $("btn-play");
    if (!btn) return;
    btn.disabled = busy;
    if (busy) {
      btn.innerHTML = "";
      var base = label || "Mise à jour…";
      if (typeof pct === "number" && isFinite(pct)) {
        btn.appendChild(document.createTextNode(base + " " + pct + "%"));
      } else {
        btn.appendChild(document.createTextNode(base));
      }
      return;
    }
    refreshPlayButton();
  }

  // État "En jeu" : bouton grisé avec l'icône play existante conservée,
  // détail repris dans `install-detail`. Appelé par `refreshPlayButton`
  // quand `gameRunning` est vrai et directement par l'événement running.
  function renderGameRunningButton() {
    var btn = $("btn-play");
    if (!btn) return;
    var svgNS = "http://www.w3.org/2000/svg";
    btn.disabled = true;
    btn.innerHTML = "";
    var svg = document.createElementNS(svgNS, "svg");
    svg.setAttribute("class", "icon icon-sm");
    var use = document.createElementNS(svgNS, "use");
    use.setAttribute("href", "#i-play");
    svg.appendChild(use);
    btn.appendChild(svg);
    btn.appendChild(document.createTextNode(" En jeu"));
    var det = $("install-detail");
    if (det) det.textContent = "Jeu en cours d'exécution.";
  }

  // Main button follows install state: INSTALLER (download icon) while
  // the game is missing/out of date, JOUER (play icon) once playable.
  function refreshPlayButton() {
    var btn = $("btn-play");
    if (!btn) return;
    if (gameRunning) {
      renderGameRunningButton();
      return;
    }
    if (btn.disabled) return;
    var need = installNeeded();
    var svgNS = "http://www.w3.org/2000/svg";
    btn.innerHTML = "";
    var svg = document.createElementNS(svgNS, "svg");
    svg.setAttribute("class", "icon icon-sm");
    var use = document.createElementNS(svgNS, "use");
    use.setAttribute("href", need ? "#i-download" : "#i-play");
    svg.appendChild(use);
    btn.appendChild(svg);
    btn.appendChild(document.createTextNode(need ? " INSTALLER" : " JOUER"));
    var det = $("install-detail");
    if (det && !playBusy && !installBusy && !gameRunning) {
      det.textContent = need
        ? "Cliquez INSTALLER pour télécharger le jeu."
        : "Prêt — cliquez JOUER, on s'occupe du reste.";
    }
  }

  // Verdict post-lancement (`running` / `exited` / `crashed`) : la carte
  // Journal reçoit toujours sa ligne de conclusion (jamais d'impasse après
  // la ligne de commande), et un plantage affiche un toast nommé. Partagé
  // entre l'écouteur événementiel et le sondage `get_last_outcome`
  // (premier signal gagne : `lastSurfacedOutcome` garde le dernier détail
  // servi, événement puis sondage — ou l'inverse — ne doublent jamais).
  var lastSurfacedOutcome = null;
  function handleGameOutcome(o) {
    o = o || {};
    var key = o.detail || (o.status + "|" + (o.cause || "") + "|" + (o.code == null ? "?" : o.code));
    if (lastSurfacedOutcome !== null && key === lastSurfacedOutcome) return;
    lastSurfacedOutcome = key;
    if (o.detail) log(o.detail);
    if (o.status === "running") {
      gameRunning = true;
      gamePid = (o.pid != null) ? o.pid : null;
      // Un jeu vivant est la vérité terrain : afficher « En jeu » sans
      // condition (les drapeaux d'opération sont de l'état d'interface,
      // jamais un veto sur un jeu en cours).
      renderGameRunningButton();
      if (gamePid != null) log("Jeu en cours (PID " + gamePid + ").");
      return;
    }
    if (o.status === "exited" || o.status === "crashed") {
      var wasRunning = gameRunning;
      gameRunning = false;
      gamePid = null;
      if (o.status === "crashed") toast("Plantage du jeu : " + (o.cause || o.detail || "cause inconnue."));
      // Opération en cours : laisser sa fin rafraîchir, sans écraser.
      if (installBusy || playBusy) return;
      // `refreshPlayButton` ignore les boutons désactivés : réactiver
      // d'abord, puis restaurer — sinon le bouton reste grisé.
      if (wasRunning) {
        var btn = $("btn-play");
        if (btn) btn.disabled = false;
        refreshPlayButton();
      }
      return;
    }
  }

  // Sondage "En jeu" (toutes les 3s, silencieux : aucun toast, juste
  // la synchronisation d'état). `running=true` affiche « En jeu » sans
  // condition (un jeu vivant est la vérité terrain) ; `false` avec
  // `gameRunning` vrai et sans opération en cours reprend le même chemin
  // de restauration que `exited` (le garde-fou d'opération ne protège que
  // cette restauration, jamais l'affichage). Le dernier verdict est aussi
  // sondé (`get_last_outcome`) : un `crashed` non encore servi passe par le
  // même `handleGameOutcome` que l'événement (toast + Journal identiques).
  // Sans garde `document.hidden` : la fenêtre est invariablement cachée ou
  // réduite PENDANT le jeu, c'est exactement là que le sondage doit tourner.
  function pollGameStatus() {
    if (!HAS_TAURI) return;
    invoke("get_game_status").then(function (st) {
      if (!st) return;
      if (st.running) {
        if (gameRunning) return;
        gameRunning = true;
        renderGameRunningButton();
        return;
      }
      if (gameRunning && !installBusy && !playBusy) {
        gameRunning = false;
        gamePid = null;
        var btn = $("btn-play");
        if (btn) btn.disabled = false;
        refreshPlayButton();
      }
    }, function () {});
    invoke("get_last_outcome").then(function (o) {
      if (!o || o.status !== "crashed") return;
      handleGameOutcome(o);
    }, function () {});
  }

  /* Secours barre par sondage (`get_install_progress`, toutes les 500 ms) :
     même patron que le sondage `game-status` — les événements restent le
     premier signal, le sondage rejoue silencieusement le dernier payload
     (aucun toast, aucun log) tant qu'une opération est active
     (`installBusy`/`playBusy`). Le garde-fou de signature évite de rejouer
     deux fois le même état. */
  var INSTALL_POLL_MS = 500;
  var installPollSig = null;

  function installProgressSig(p) {
    return [p.completed_files, p.total_files, p.completed_bytes, p.total_bytes, p.fraction, p.detail, p.current_file].join("|");
  }
  function pollInstallProgress() {
    if (!HAS_TAURI) return;
    if (!installBusy && !playBusy) { installPollSig = null; return; }
    invoke("get_install_progress").then(function (p) {
      if (!p) return;
      var sig = installProgressSig(p);
      if (sig === installPollSig) return;
      installPollSig = sig;
      try { renderProgress(p); } catch (e) {}
    }, function () {});
  }
  /* File console par sondage (`get_console_tail`, toutes les 1s) : même
     patron que les autres secours — les événements `console-line` restent
     le premier signal, le sondage rejoue silencieusement (aucun toast) les
     lignes manquées via le même `glog` (dédupliquées par curseur, bornées
     par le cap existant). Sondé uniquement quand c'est utile : jeu en cours
     OU onglet Console visible. SANS garde `document.hidden` : le jeu tourne
     fenêtre cachée, et le journal doit être complet au retour. */
  var consoleTailCursor = 0;
  function isConsoleVisible() {
    var t = $("tab-console");
    return !!(t && !t.classList.contains("hidden"));
  }
  function pollConsoleTail() {
    if (!HAS_TAURI) return;
    if (!gameRunning && !isConsoleVisible()) return;
    invoke("get_console_tail", { since: consoleTailCursor }).then(function (t) {
      if (!t) return;
      var lines = t.lines || [];
      for (var i = 0; i < lines.length; i++) {
        try { glog(lines[i]); } catch (e) {}
      }
      if (typeof t.next === "number") consoleTailCursor = t.next;
    }, function () {});
  }

  /* Session par sondage (`get_session`, toutes les 30s) : remplace la
     dépendance à l'événement `session-changed` (restauration silencieuse au
     boot qui atterrit après le premier affichage). Silencieux sans
     changement : le diff nom/uuid décide, puis le même `renderSession` +
     le même toast que le chemin événementiel. L'écouteur est conservé
     (la même vérification rend le doublon inoffensif). */
  function sessionKey(s) {
    return s ? (s.uuid + "|" + s.name) : "null";
  }
  function applySessionUpdate(s) {
    if (sessionKey(s) === sessionKey(sessionCached)) return;
    if (!s) {
      renderSession(s);
      return;
    }
    bootAuthResolved = true;
    renderSession(s);
    refreshAccounts();
    toast("Session restaurée : " + s.name + ".");
  }
  function pollSession() {
    if (!HAS_TAURI || document.hidden) return;
    invoke("get_session").then(function (s) {
      applySessionUpdate(s);
    }, function () {});
  }

  /* Vitesse côté client de la modale d'installation (aucun changement
     backend : `detail` reste mot pour mot du backend, la vitesse vit dans
     son propre élément). Lissée sur ~1s (jitter sous la seconde ignoré),
     `—` quand les octets stagnent > 2s (hachage, extraction…),
     réinitialisée à chaque run. */
  var installSpeedAnchor = null; // { b, t } — ancre de la fenêtre ~1s
  var installSpeedShown = null; // derniers octets/s affichés
  var installSpeedStallAt = 0; // dernier instant où les octets ont bougé
  var installPrevBytes = null; // derniers octets vus
  var installSeenFile = false; // un événement a déjà alimenté la modale
  var installPrevFraction = 0; // détection de nouveau run (retour vers 0)

  function installNowMs() {
    return (typeof performance !== "undefined" && performance.now)
      ? performance.now()
      : Date.now();
  }

  // L'unique ajout DOM de la modale : un petit élément vitesse coincé après
  // le pct (la ligne `detail` reste mot pour mot du backend).
  function installSpeedEl() {
    var el = $("install-modal-speed");
    if (el) return el;
    var pct = $("install-modal-pct");
    if (!pct || !pct.parentNode) return null;
    el = document.createElement("span");
    el.id = "install-modal-speed";
    el.className = "pct";
    el.style.fontSize = "11px";
    el.style.opacity = "0.75";
    el.style.marginLeft = "8px";
    el.textContent = "—";
    pct.parentNode.insertBefore(el, pct.nextSibling);
    return el;
  }

  function formatSpeedFr(bytesPerSec) {
    return "▲ " + formatBytes(bytesPerSec).replace(".", ",") + "/s";
  }

  function resetInstallSpeed() {
    installSpeedAnchor = null;
    installSpeedShown = null;
    installSpeedStallAt = 0;
    installPrevBytes = null;
    installSeenFile = false;
    installPrevFraction = 0;
    var el = $("install-modal-speed");
    if (el) el.textContent = "—";
  }

  function updateInstallSpeed(p) {
    var el = installSpeedEl();
    var now = installNowMs();
    var bytes = Number(p.completed_bytes);
    if (!isFinite(bytes) || bytes < 0) bytes = 0;
    // Nouveau run sans réouverture (fraction retombée près de 0 après 1.0,
    // ou compteurs repartis) : réancrer la vitesse, mais la modale reste en
    // plein run (`installSeenFile` inchangé).
    if (((p.fraction || 0) < 0.02 && installPrevFraction > 0.9)
        || (installPrevBytes != null && bytes < installPrevBytes)) {
      installSpeedAnchor = null;
      installSpeedShown = null;
      installSpeedStallAt = now;
      installPrevFraction = p.fraction || 0;
      installPrevBytes = bytes;
      if (el) el.textContent = "—";
      return;
    }
    installPrevFraction = p.fraction || 0;
    if (bytes !== installPrevBytes) installSpeedStallAt = now;
    installPrevBytes = bytes;
    if (!installSpeedAnchor) {
      installSpeedAnchor = { b: bytes, t: now };
      if (el && installSpeedShown == null) el.textContent = "—";
      return;
    }
    var dt = now - installSpeedAnchor.t;
    if (dt < 1000) {
      // Jitter sous la seconde : on garde l'affichage ; postage du stall.
      if (now - installSpeedStallAt > 2000 && installSpeedShown != null) {
        installSpeedShown = null;
        if (el) el.textContent = "—";
      }
      return;
    }
    var db = bytes - installSpeedAnchor.b;
    if (dt <= 3000 && db > 0) {
      installSpeedShown = db / (dt / 1000);
      if (el) el.textContent = formatSpeedFr(installSpeedShown);
    } else if (now - installSpeedStallAt > 2000) {
      // Fenêtre vide ou reprise après un long silence : `—`, sans calculer
      // une vitesse sur une fenêtre éventée.
      installSpeedShown = null;
      if (el) el.textContent = "—";
    }
    installSpeedAnchor = { b: bytes, t: now };
  }

  // Garde-fou de signature au rendu (pas seulement au sondage) : le même
  // payload livré deux fois (événement + sondage 500 ms) n'écrit dans le
  // DOM qu'une fois.
  var lastRenderProgressSig = null;
  function renderProgress(p) {
    var sig = installProgressSig(p);
    if (sig === lastRenderProgressSig) return;
    lastRenderProgressSig = sig;
    var pct = Math.round((p.fraction || 0) * 100);
    var bar = $("install-bar"), det = $("install-detail");
    if (bar) bar.style.width = pct + "%";
    if (det) det.textContent = p.detail || (pct + "%");
    var mb = $("install-modal-bar"), md = $("install-modal-detail"), mp = $("install-modal-pct");
    if (mb) mb.style.width = pct + "%";
    if (md) md.textContent = p.detail || (pct + "%");
    if (mp) mp.textContent = pct + "%";
    updateInstallSpeed(p);
    var mf = $("install-modal-file");
    // `current_file` vide = action sans fichier (vérification, extraction…) :
    // tirer `—`, jamais un nom périmé. `Préparation…` ne reste que l'état
    // avant le premier événement (voir `showInstallModal`).
    if (mf) {
      if (p.current_file) {
        mf.textContent = p.current_file;
        installSeenFile = true;
      } else if (installSeenFile) mf.textContent = "—";
      else mf.textContent = "Préparation…";
    }
    var hb = $("hero-bar"), hp = $("hero-progress"), hd = $("hero-detail"), hpc = $("hero-pct");
    if (hb && hp) {
      hb.style.width = pct + "%";
      hp.classList.remove("hidden");
      if (hd) hd.textContent = p.detail || (pct + "%");
      if (hpc) hpc.textContent = pct + "%";
    }
    // Installation : "Mise à jour… pct%" ; lancement : garder "Lancement…"
    // (la progression ne doit pas réécrire le libellé du lancement).
    if (installBusy) setPlayBusy(true, pct);
    else if (playBusy) setPlayBusy(true, null, "Lancement…");
  }

  function hideHeroProgress() {
    var hp = $("hero-progress");
    if (hp) hp.classList.add("hidden");
  }

  function showInstallModal() {
    // `resetInstallSpeed` remet `installSeenFile` à faux, puis le pré-rendu
    // ci-dessous le repasse à vrai : tout événement suivant est en plein run
    // (`""` → `—`, jamais `Préparation…`).
    resetInstallSpeed();
    renderProgress({ fraction: 0, detail: "Préparation…", current_file: "Préparation…" });
    var m = $("install-modal");
    if (m) m.classList.remove("hidden");
    var t = $("install-modal-title");
    if (t) t.textContent = "Installation en cours…";
    var c = $("btn-install-modal-cancel");
    if (c) {
      c.textContent = "Annuler";
      c.onclick = function () { doCancelInstall(); };
    }
  }
  // Completion state: summary stays on screen with a dismiss button
  // instead of vanishing instantly (no-op installs looked broken).
  function installModalDone(detail) {
    var md = $("install-modal-detail");
    if (md) md.textContent = detail;
    var mf = $("install-modal-file");
    if (mf) mf.textContent = "Terminé";
    var mb = $("install-modal-bar");
    if (mb) mb.style.width = "100%";
    var mp = $("install-modal-pct");
    if (mp) mp.textContent = "100%";
    var c = $("btn-install-modal-cancel");
    if (c) {
      c.textContent = "Fermer";
      c.onclick = function () { hideInstallModal(); };
    }
  }
  function hideInstallModal() {
    var m = $("install-modal");
    if (m) m.classList.add("hidden");
  }

  /* ---------- install options (Loki opt-in) ---------- */

  var lokiPendingMode = "install";
  var lastSettings = {};

  // Pack flavor (default vs experimental modernized): single writer used
  // by the install-options modal dropdown. Fetches live settings, flips both flags +
  // the matching canonical JVM args (hard-set per flavor), persists, then
  // calls the backend `set_pack_flavor` swap (modern stack stashed/restored
  // immediately, moves logged to the Console) and re-syncs every UI
  // surface (modal dropdown + home readonly line). The runtime (Temurin 8 / GraalVM 21) derives from the flavor
  // at use time — no setting is written for it. The install backend reads
  // these live.
  var JVM_ARGS_DEFAULT = "46anarchy";
  // Base ZGC expérimentale = preset « graal-zgc » (source unique).
  var JVM_ARGS_EXPERIMENTAL = jvmPresetArgs("graal-zgc");
  // Interrupteur unique de la branche expérimentale (miroir de
  // `EXPERIMENTAL_GATED` backend, `crates/ui/src/state.rs`) : `true` =
  // entrée visible mais désactivée + sélection refusée + migration au
  // démarrage. Un seul basculement réactive la branche.
  var EXPERIMENTAL_GATED = true;
  var EXPERIMENTAL_GATED_LABEL = "Expérimental modernisé (bientôt disponible)";
  var EXPERIMENTAL_GATED_SUB = "Bientôt disponible — le modpack Paladium reste sélectionné";
  var EXPERIMENTAL_GATED_TIP = "Branche expérimentale temporairement désactivée — le modpack Paladium reste disponible.";
  var EXPERIMENTAL_GATED_TOAST = "Branche expérimentale indisponible pour le moment — modpack Paladium conservé.";
  var PACK_FLAVORS = {
    "default": "Modpack Paladium (Java 8)",
    "experimental": "Expérimental modernisé (Java 21, Beddium, Computium)"
  };
  var PACK_FLAVOR_SUB = {
    "default": "Java 8 — pack Paladium classique",
    "experimental": "Java 21 + Beddium + Computium — runtime géré automatiquement"
  };
  function setPackFlavor(value, after) {
    // Garde verrou (doublure du bouton désactivé + du refus backend) :
    // toute tentative vers l'expérimental est refusée sans rien persister.
    if (EXPERIMENTAL_GATED && value === "experimental") {
      toast(EXPERIMENTAL_GATED_TOAST);
      syncPackFlavorUI();
      if (after) after();
      return;
    }
    var wantExp = value === "experimental";
    invoke("get_settings").then(function (s) {
      s = s || {};
      lastSettings = s;
      var cur = !!s.experimental_runtime || !!s.experimental_optimizations;
      if (cur === wantExp) {
        syncPackFlavorUI();
        refreshJvmPresetLock(false);
        if (after) after();
        return;
      }
      s.experimental_runtime = wantExp;
      s.experimental_optimizations = wantExp;
      s.jvm_preset = wantExp ? "graal-zgc" : JVM_ARGS_DEFAULT;
      s.jvm_args = wantExp ? JVM_ARGS_EXPERIMENTAL : (jvmPresetArgs(JVM_ARGS_DEFAULT) || "");
      invoke("save_settings", { settings: s }).then(function (saved) {
        lastSettings = saved || s;
        try { fillSettings(lastSettings); } catch (e) {}
        syncPackFlavorUI();
        fetchJavaStatus();
        renderJdkInfoCard();
        fetchJavaRuntimes();
        toast(wantExp ? "Version expérimentale sélectionnée (Java 21, Beddium, Computium)." : "Modpack Paladium sélectionné.");
        // Échange immédiat du stock de mods (backend `set_pack_flavor`) :
        // la pile moderne est mise de côté / restaurée sans attendre la
        // prochaine installation ; chaque déplacement est journalisé.
        invoke("set_pack_flavor", { experimental: wantExp }).then(function (swap) {
          try {
            var lines = (swap && swap.lines) || [];
            for (var i = 0; i < lines.length; i++) glog(lines[i]);
          } catch (e) {}
          if (after) after();
        }, function () { if (after) after(); });
      }, function (err) {
        toast(typeof err === "string" ? err : "Réglages inaccessibles.");
      });
    }, function (err) {
      toast(typeof err === "string" ? err : "Réglages inaccessibles.");
    });
  }

  // Ligne « GC effectif » (lecture seule) : miroir statique de la flavor —
  // expérimental → ZGC, classique → G1GC. Même source que l'argv forcé
  // (`currentFlavor`), rafraîchie via `syncPackFlavorUI` (bascule flavor +
  // remplissage au démarrage).
  function refreshGcEffectif() {
    var el = $("jvm-gc-effectif");
    if (!el) return;
    var exp = currentFlavor() === "experimental";
    el.textContent = "Collecteur GC appliqué au lancement : " + (exp ? "ZGC" : "G1GC");
  }

  function syncPackFlavorUI() {
    var exp = !!(lastSettings.experimental_runtime || lastSettings.experimental_optimizations);
    var value = exp ? "experimental" : "default";
    var text = PACK_FLAVORS[value] || value;
    var label = $("pack-flavor-label");
    if (label) label.textContent = text;
    var current = $("pack-flavor-current");
    if (current) current.textContent = text;
    var menu = $("pack-flavor-menu");
    if (menu) {
      var items = menu.querySelectorAll("button[data-flavor]");
      for (var i = 0; i < items.length; i++) {
        items[i].classList.toggle("active", items[i].getAttribute("data-flavor") === value);
      }
      // Verrou expérimental : entrée visible mais désactivée, libellé
      // « bientôt disponible » + infobulle explicative. Le basculement de
      // `EXPERIMENTAL_GATED` restaure l'entrée d'origine sans autre retouche.
      var expBtn = menu.querySelector('button[data-flavor="experimental"]');
      if (expBtn) {
        var main = expBtn.querySelector(".flavor-main");
        var sub = expBtn.querySelector("small");
        if (EXPERIMENTAL_GATED) {
          expBtn.disabled = true;
          expBtn.classList.remove("active");
          expBtn.title = EXPERIMENTAL_GATED_TIP;
          if (main) main.textContent = EXPERIMENTAL_GATED_LABEL;
          if (sub) sub.textContent = EXPERIMENTAL_GATED_SUB;
        } else {
          expBtn.disabled = false;
          expBtn.removeAttribute("title");
          if (main) main.textContent = PACK_FLAVORS.experimental;
          if (sub) sub.textContent = PACK_FLAVOR_SUB.experimental;
        }
      }
    }
    refreshGcEffectif();
  }

  function togglePackFlavorMenu(force) {
    var menu = $("pack-flavor-menu");
    if (!menu) return;
    var show = typeof force === "boolean" ? force : menu.classList.contains("hidden");
    menu.classList.toggle("hidden", !show);
    var btn = $("pack-flavor-btn");
    if (btn) btn.setAttribute("aria-expanded", show ? "true" : "false");
  }

  function openLokiModal(mode) {
    lokiPendingMode = mode || "install";
    var cb = $("loki-checkbox");
    if (cb) cb.checked = false;
    renderJdkInfoCard();
    fetchJavaRuntimes();
    var m = $("loki-modal");
    if (m) m.classList.remove("hidden");
    var confirm = $("btn-loki-install");
    if (confirm) confirm.focus();
  }
  function closeLokiModal() {
    var m = $("loki-modal");
    if (m) m.classList.add("hidden");
  }
  function cancelLokiModal() {
    closeLokiModal();
  }
  function confirmLokiModal() {
    var cb = $("loki-checkbox");
    var flag = cb ? !!cb.checked : false;
    var mode = lokiPendingMode || "install";
    closeLokiModal();
    proceedAfterPackChoice(mode, flag);
  }
  // Plus de lancement automatique après installation : INSTALLER installe
  // et vérifie uniquement (modale fermée, toast, statut JOUER ensuite).
  // Seul JOUER (doPlay → backend `play`, avec sa mise à niveau si périmé)
  // lance le jeu. Le mode "play" historique rejoint donc le même flux.
  function proceedAfterPackChoice(mode, flag) {
    doInstall(flag);
  }

  function installNeeded() {
    if (!lastStatus) return true;
    return lastStatus.update_status === "init" || lastStatus.update_status === "out_of_date";
  }

  /* ---------- accounts (real `get_accounts` / `set_active_account` / `refresh_account` / `remove_account`) ---------- */

  function refreshAccounts() {
    if (!requireBackend()) return;
    invoke("get_accounts").then(function (list) {
      renderAccountsList(list || []);
      renderAccountMenu(list || []);
    }, function () {
      renderAccountsList([]);
      renderAccountMenu([]);
    });
  }

  function renderAccountsList(list) {
    var box = $("accounts-list");
    if (!box) return;
    box.innerHTML = "";
    if (!list.length) {
      var p = document.createElement("p");
      p.className = "muted";
      p.textContent = "Aucun compte connecté.";
      box.appendChild(p);
      return;
    }
    for (var i = 0; i < list.length; i++) (function (a) {
      var row = document.createElement("div");
      row.className = "account-row";
      row.appendChild(avatarMini(a.uuid, false));
      var who = document.createElement("div");
      who.className = "who";
      var nm = document.createElement("strong");
      nm.textContent = a.name;
      who.appendChild(nm);
      var id = document.createElement("small");
      id.textContent = a.uuid;
      who.appendChild(id);
      row.appendChild(who);
      if (a.active) {
        var badge = document.createElement("span");
        badge.className = "active-badge";
        badge.textContent = "Actif";
        row.appendChild(badge);
      }
      var rf = document.createElement("button");
      rf.className = "btn btn-secondary btn-small";
      rf.type = "button";
      rf.textContent = "Actualiser";
      rf.addEventListener("click", function () { refreshAccount(a.uuid, a.name); });
      row.appendChild(rf);
      var rm = document.createElement("button");
      rm.className = "btn btn-ghost btn-small";
      rm.type = "button";
      rm.textContent = "Retirer";
      rm.addEventListener("click", function () { removeAccount(a.uuid, a.name); });
      row.appendChild(rm);
      box.appendChild(row);
    })(list[i]);
  }

  function renderAccountMenu(list) {
    var menu = $("account-menu");
    if (!menu) return;
    menu.innerHTML = "";
    for (var i = 0; i < list.length; i++) (function (a) {
      var b = document.createElement("button");
      b.type = "button";
      if (a.active) b.className = "active";
      b.appendChild(avatarMini(a.uuid, true));
      b.appendChild(document.createTextNode(a.name + (a.active ? " ✓" : "")));
      b.addEventListener("click", function (ev) {
        ev.stopPropagation();
        toggleAccountMenu(false);
        if (!a.active) switchAccount(a.uuid);
      });
      menu.appendChild(b);
    })(list[i]);
    if (list.length) {
      var div = document.createElement("div");
      div.className = "divider";
      div.style.margin = "6px 4px";
      menu.appendChild(div);
    }
    var out = document.createElement("button");
    out.type = "button";
    var osvgNS = "http://www.w3.org/2000/svg";
    var osvg = document.createElementNS(osvgNS, "svg");
    osvg.setAttribute("class", "icon icon-sm");
    var ouse = document.createElementNS(osvgNS, "use");
    ouse.setAttribute("href", "#i-logout");
    osvg.appendChild(ouse);
    out.appendChild(osvg);
    out.appendChild(document.createTextNode("Déconnexion"));
    out.addEventListener("click", function (ev) {
      ev.stopPropagation();
      doLogout();
    });
    menu.appendChild(out);
  }

  function doLogout() {
    toggleAccountMenu(false);
    if (!requireBackend()) return;
    invoke("logout").then(function () {
      sessionCached = null;
      refreshAll();
      toast("Déconnecté.");
    }, function (err) { toast(typeof err === "string" ? err : "Erreur."); });
  }

  function toggleAccountMenu(force) {
    var menu = $("account-menu");
    if (!menu) return;
    var show = typeof force === "boolean" ? force : menu.classList.contains("hidden");
    menu.classList.toggle("hidden", !show);
  }

  function switchAccount(uuid) {
    if (!requireBackend()) return;
    invoke("set_active_account", { uuid: uuid }).then(function (session) {
      renderSession(session);
      refreshAccounts();
      toast("Compte actif : " + session.name + ".");
    }, function (err) {
      toast(typeof err === "string" ? err : "Changement de compte impossible.");
      refreshAccounts();
    });
  }

  function refreshAccount(uuid, name) {
    if (!requireBackend()) return;
    invoke("refresh_account", { uuid: uuid }).then(function (account) {
      sessionCached = null;
      refreshAll();
      toast("Session actualisée : " + (account && account.name ? account.name : name) + ".");
    }, function (err) {
      toast(typeof err === "string" ? err : "Actualisation impossible.");
    });
  }

  function removeAccount(uuid, name) {
    if (!requireBackend()) return;
    invoke("remove_account", { uuid: uuid }).then(function (ok) {
      if (ok) {
        toast("Compte retiré : " + name + ".");
        // refreshAll re-polls: fallback active renders, or a genuine
        // last-account null flows through (no stale cache involved —
        // the poll result is authoritative either way).
        sessionCached = null;
        refreshAll();
      } else {
        toast("Compte introuvable.");
      }
    }, function (err) {
      toast(typeof err === "string" ? err : "Retrait impossible.");
    });
  }

  /* ---------- mods manager (real `get_mods_list` / `toggle_mod` only) ---------- */

  var modsCache = [];
  var modsFilter = "all";

  // Loading state -> real list -> counts from `modsCache.length`. Errors show
  // the backend message, never placeholder rows.
  // Revisite d'onglet sans refetch quand les données sont fraîches ;
  // `applyModsFilter` réaffiche le cache (garde de signature ci-dessous).
  var lastModsFetchAt = 0;
  var MODS_FRESH_MS = 15000;
  var lastModsRenderSig = null;
  function modsRenderSig(q) {
    var parts = [modsFilter, q, "n" + modsCache.length];
    for (var i = 0; i < modsCache.length; i++) {
      var m = modsCache[i] || {};
      parts.push([m.id || "", m.enabled ? "1" : "0", m.required ? "1" : "0", m.version || "", m.size || 0, m.display_name || "", m.file_name || ""].join("~"));
    }
    return parts.join("|");
  }
  function refreshMods(force) {
    if (!HAS_TAURI) { showNoBackendMode(); return; }
    if (!force && (Date.now() - lastModsFetchAt) < MODS_FRESH_MS && modsCache.length) {
      applyModsFilter();
      return;
    }
    var sub = $("mods-subtitle");
    if (sub) sub.textContent = "Chargement des mods…";
    lastModsFetchAt = Date.now();
    invoke("get_mods_list").then(function (mods) {
      modsCache = mods || [];
      applyModsFilter();
      updateHomeModLine();
    }, function (err) {
      modsCache = [];
      lastModsRenderSig = null;
      var list = $("mods-list");
      if (list) list.innerHTML = "";
      if (sub) sub.textContent = typeof err === "string" ? err : "Mods injoignables.";
      var sc = $("mods-search-count");
      if (sc) sc.textContent = "0 mod";
      setCount("count-all", 0);
      setCount("count-required", 0);
      setCount("count-optional", 0);
      setCount("count-disabled", 0);
    });
  }

  function setCount(id, v) {
    var el = $(id);
    if (el) el.textContent = String(v);
  }

  // Search + Tous/Obligatoires/Optionnels/Désactivés operate on the real list.
  function applyModsFilter() {
    var q = "";
    var search = $("mods-search");
    if (search) q = (search.value || "").toLowerCase().trim();
    var rsig = modsRenderSig(q);
    if (rsig === lastModsRenderSig) return;
    lastModsRenderSig = rsig;
    var counts = { all: modsCache.length, required: 0, optional: 0, disabled: 0 };
    modsCache.forEach(function (m) {
      if (m.required) counts.required += 1;
      else counts.optional += 1;
      if (!m.enabled) counts.disabled += 1;
    });
    setCount("count-all", counts.all);
    setCount("count-required", counts.required);
    setCount("count-optional", counts.optional);
    setCount("count-disabled", counts.disabled);
    var sub = $("mods-subtitle");
    if (sub) {
      sub.textContent = counts.all === 0
        ? "Aucun mod installé"
        : (counts.all + (counts.all > 1 ? " mods installés" : " mod installé"));
    }
    var sc = $("mods-search-count");
    var list = $("mods-list");
    if (!list) return;
    list.innerHTML = "";
    var visible = 0;
    modsCache.forEach(function (m) {
      var hay = ((m.display_name || "") + " " + (m.file_name || "") + " " + (m.id || "")).toLowerCase();
      if (q && hay.indexOf(q) === -1) return;
      if (modsFilter === "required" && !m.required) return;
      if (modsFilter === "optional" && m.required) return;
      if (modsFilter === "disabled" && m.enabled) return;
      visible += 1;
      list.appendChild(modRow(m));
    });
    if (sc) sc.textContent = visible + (visible > 1 ? " mods" : " mod");
    if (visible === 0) {
      var empty = document.createElement("div");
      empty.className = "mod-row";
      var main = document.createElement("div");
      main.className = "mod-main";
      var span = document.createElement("span");
      span.className = "muted";
      span.style.fontSize = "12px";
      span.textContent = modsCache.length === 0 ? "Aucun mod installé." : "Aucun mod ne correspond au filtre.";
      main.appendChild(span);
      empty.appendChild(main);
      list.appendChild(empty);
    }
  }

  // One row per real ModInfo: name, version, size, required badge / toggle.
  function modRow(m) {
    var row = document.createElement("div");
    row.className = "mod-row";
    var main = document.createElement("div");
    main.className = "mod-main";
    var icon = document.createElement("span");
    icon.className = "mod-icon" + (m.required ? " req" : "");
    var svgNS = "http://www.w3.org/2000/svg";
    var svg = document.createElementNS(svgNS, "svg");
    svg.setAttribute("class", "icon");
    var use = document.createElementNS(svgNS, "use");
    use.setAttribute("href", m.required ? "#i-shield" : "#i-box");
    svg.appendChild(use);
    icon.appendChild(svg);
    main.appendChild(icon);
    var txt = document.createElement("div");
    txt.style.minWidth = "0";
    var name = document.createElement("strong");
    name.textContent = m.display_name || m.file_name || m.id;
    name.title = (m.file_name || m.id);
    var desc = document.createElement("small");
    desc.textContent = m.description || (m.required ? "Requis par le serveur de jeu" : "Mod optionnel local");
    txt.appendChild(name);
    txt.appendChild(desc);
    main.appendChild(txt);
    row.appendChild(main);
    var ver = document.createElement("div");
    ver.className = "mod-ver hide-m";
    ver.textContent = m.version || "—";
    row.appendChild(ver);
    var size = document.createElement("div");
    size.className = "mod-size hide-m";
    size.textContent = formatBytes(m.size || 0);
    row.appendChild(size);
    var status = document.createElement("div");
    status.className = "mod-status";
    if (m.required) {
      var badge = document.createElement("span");
      badge.className = "req-badge";
      badge.title = "Requis par le serveur de jeu";
      var lsvg = document.createElementNS(svgNS, "svg");
      lsvg.setAttribute("class", "icon");
      var luse = document.createElementNS(svgNS, "use");
      luse.setAttribute("href", "#i-lock");
      lsvg.appendChild(luse);
      badge.appendChild(lsvg);
      badge.appendChild(document.createTextNode("Obligatoire"));
      status.appendChild(badge);
    } else {
      var t = document.createElement("button");
      t.type = "button";
      t.className = "toggle";
      t.setAttribute("role", "switch");
      t.setAttribute("aria-checked", m.enabled ? "true" : "false");
      t.title = m.enabled ? "Désactiver le mod" : "Activer le mod";
      if (!HAS_TAURI) t.disabled = true;
      var knob = document.createElement("span");
      knob.className = "knob";
      t.appendChild(knob);
      (function (mod, btn) {
        btn.addEventListener("click", function () {
          if (!requireBackend()) return;
          var want = btn.getAttribute("aria-checked") !== "true";
          btn.disabled = true;
          invoke("toggle_mod", { id: mod.id, enabled: want }).then(function () {
            toast((want ? "Activé : " : "Désactivé : ") + mod.id);
            refreshMods(true);
          }, function (err) {
            btn.disabled = false;
            toast(typeof err === "string" ? err : "Échec du changement d'état.");
          });
        });
      })(m, t);
      status.appendChild(t);
    }
    row.appendChild(status);
    return row;
  }

  /* ---------- java config screen (real `get_settings` / `save_settings`) ---------- */

  // Slider ceiling: backend physical RAM, 12 GiB fallback until fetched.
  var RAM_MAX_MB = 12288;
  function ramMbFromSettings(s) {
    var raw = ((s && s.memory_max) || "4G").trim().toUpperCase();
    var m = raw.match(/^(\d+(?:\.\d+)?)\s*(G|M)?/);
    if (!m) return 4096;
    var v = parseFloat(m[1]);
    if (m[2] === "M") return Math.round(v);
    return Math.round(v * 1024);
  }

  function setRam(mb) {
    var range = $("ram-range");
    var val = Math.min(RAM_MAX_MB, Math.max(1024, Math.round(mb / 512) * 512));
    if (range) {
      range.max = String(RAM_MAX_MB);
      range.value = String(val);
      range.style.setProperty("--fill", ((val - 1024) / (RAM_MAX_MB - 1024) * 100) + "%");
    }
    var gb = val / 1024;
    var gbLabel = (Number.isInteger(gb) ? String(gb) : gb.toFixed(1)) + " Go";
    var dg = $("ram-display-gb"), dm = $("ram-display-mb");
    if (dg) dg.textContent = gbLabel;
    if (dm) dm.textContent = val.toLocaleString("fr-FR");
    var fv = $("ram-footer-val"), fb = $("ram-footer-bar");
    if (fv) fv.textContent = val.toLocaleString("fr-FR") + " Mo";
    if (fb) fb.style.width = Math.round((val / RAM_MAX_MB) * 100) + "%";
    var btns = document.querySelectorAll("#ram-presets .preset-btn");
    for (var i = 0; i < btns.length; i++) {
      btns[i].classList.toggle("active", parseInt(btns[i].getAttribute("data-ram"), 10) === val);
    }
    renderRamScaleAndPresets();
  }

  function fmtGoMb(mb) {
    var gb = mb / 1024;
    return (Number.isInteger(gb) ? String(gb) : gb.toFixed(1)) + " Go";
  }

  // Échelle (min / milieu / max réels du slider) + verrou des raccourcis
  // au-dessus du plafond système. Rendu après chaque `setRam`, donc sur
  // tous les chemins qui touchent au max (boot, `fetchSystemMemory`).
  function renderRamScaleAndPresets() {
    var range = $("ram-range");
    var min = range ? (parseInt(range.min, 10) || 1024) : 1024;
    var max = RAM_MAX_MB;
    var mid = Math.round((min + max) / 2 / 512) * 512;
    var scale = $("ram-scale");
    if (scale) {
      scale.innerHTML = "";
      [min, mid, max].forEach(function (v) {
        var s = document.createElement("span");
        s.textContent = fmtGoMb(v);
        scale.appendChild(s);
      });
    }
    var btns = document.querySelectorAll("#ram-presets .preset-btn");
    for (var i = 0; i < btns.length; i++) {
      (function (b) {
        var need = parseInt(b.getAttribute("data-ram"), 10) || 0;
        var over = need > max;
        b.disabled = over;
        if (over) {
          b.title = "Nécessite " + fmtGoMb(need) + " — maximum " + fmtGoMb(max) + " sur cette machine.";
        } else if (need === 4096) {
          b.title = "Valeur par défaut recommandée.";
        } else {
          b.removeAttribute("title");
        }
        if (need === 4096) {
          b.textContent = over ? "4 Go" : "4 Go (Défaut)";
        }
      })(btns[i]);
    }
  }

  // System RAM ceiling (once per boot; re-clamps the current setting).
  function fetchSystemMemory() {
    if (!HAS_TAURI) { renderRamScaleAndPresets(); return; }
    invoke("get_system_memory").then(function (mb) {
      mb = Math.max(2048, Math.min(1048576, parseInt(mb, 10) || 12288));
      RAM_MAX_MB = mb;
      var cur = ramMbFromSettings(lastSettings);
      setRam(cur);
    }, function () { renderRamScaleAndPresets(); });
  }

  // Sauvegarde automatique des réglages Java (plus de bouton Enregistrer) :
  // texte/textarea débouncés ~500ms, cases/menus/boutons immédiats, slider
  // RAM à la relâche (`change`, `input` = étiquette seule). Succès muet,
  // UN toast en échec. Le DOM n'est jamais réécrit en succès (pas de vol
  // de frappe), `lastSettings` suit en mémoire.
  var settingsSaveTimer = null;
  function persistSettingsNow(onOk) {
    if (!HAS_TAURI) return;
    invoke("save_settings", { settings: collectSettings() }).then(function (s) {
      if (s) lastSettings = s;
      if (onOk) onOk();
    }, function (err) {
      toast(typeof err === "string" ? err : "Échec d'enregistrement.");
    });
  }
  function scheduleSettingsSave() {
    if (settingsSaveTimer) clearTimeout(settingsSaveTimer);
    settingsSaveTimer = setTimeout(function () {
      settingsSaveTimer = null;
      persistSettingsNow();
    }, 500);
  }

  // Onglets JVM resynchronisés au texte (frappe manuelle = aucun onglet
  // actif, jamais d'écrasement). Les onglets verrouillés restent inactifs.
  function syncJvmTabsToText() {
    var area = $("jvm-args");
    var cur = area ? area.value : "";
    var tabs = document.querySelectorAll("#jvm-presets button");
    for (var i = 0; i < tabs.length; i++) {
      (function (b) {
        if (b.disabled) { b.classList.remove("active"); return; }
        b.classList.toggle("active", cur === jvmPresetArgs(b.getAttribute("data-preset")));
      })(tabs[i]);
    }
  }

  function applyJvmPreset(name) {
    var btn = document.querySelector('#jvm-presets button[data-preset="' + name + '"]');
    // Clic sur un preset verrouillé (flavor adverse) : sans effet.
    if (btn && btn.disabled) return;
    var area = $("jvm-args");
    var args = jvmPresetArgs(name);
    if (area && args) area.value = args;
    var tabs = document.querySelectorAll("#jvm-presets button");
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].classList.toggle("active", tabs[i].getAttribute("data-preset") === name);
    }
    if (name === "lowram" || name === "graal-lowram") setRam(2048);
  }

  var JVM_DEFAULT_FOR_FLAVOR = { "default": "46anarchy", "experimental": "graal-zgc" };

  function currentFlavor() {
    return (lastSettings.experimental_runtime || lastSettings.experimental_optimizations) ? "experimental" : "default";
  }

  function presetFlavors(name) {
    var p = JVM_PRESETS[name];
    if (!p || typeof p === "string") return ["default", "experimental"];
    return p.flavors || ["default", "experimental"];
  }

  // Verrouille les presets hors flavor (attribut + classe + titre) et
  // re-sélectionne le preset canonique si la sélection devient invalide.
  // Une zone custom (aucun onglet actif, ou texte modifié à la main) n'est
  // jamais écrasée. Appelé sur changement de flavor et au boot.
  function refreshJvmPresetLock(autoSwitch) {
    var flavor = currentFlavor();
    var tabs = document.querySelectorAll("#jvm-presets button");
    for (var i = 0; i < tabs.length; i++) {
      (function (b) {
        var name = b.getAttribute("data-preset");
        var ok = presetFlavors(name).indexOf(flavor) !== -1;
        b.disabled = !ok;
        b.classList.toggle("locked", !ok);
        if (!ok) {
          b.title = flavor === "experimental"
            ? "Réservé au modpack Paladium (Java 8)."
            : "Réservé au modpack expérimental (Java 21).";
        } else {
          b.removeAttribute("title");
        }
      })(tabs[i]);
    }
    if (autoSwitch === false) return;
    var active = document.querySelector("#jvm-presets button.active");
    if (!active || !active.disabled) return;
    var area = $("jvm-args");
    var cur = active.getAttribute("data-preset");
    if (area && area.value !== jvmPresetArgs(cur)) return;
    applyJvmPreset(JVM_DEFAULT_FOR_FLAVOR[flavor] || "46anarchy");
  }

  function currentJvmPreset() {
    var active = document.querySelector("#jvm-presets button.active");
    return active ? active.getAttribute("data-preset") : "46anarchy";
  }

  // Résolution : même patron bouton+menu que le sélecteur de modpack
  // (`pack-flavor-btn`/`pack-flavor-menu`), persistée via `resolution`.
  var RES_OPTIONS = {
    "1920x1080": "1920 × 1080 (1080p FHD - Recommandé)",
    "2560x1440": "2560 × 1440 (1440p QHD)",
    "1280x720": "1280 × 720 (720p HD)",
    "3840x2160": "3840 × 2160 (4K UHD)",
    "1600x900": "1600 × 900 (900p HD+)",
    "1366x768": "1366 × 768 (Portable)"
  };
  var RES_DEFAULT = "1920x1080";
  var currentResolution = RES_DEFAULT;

  function setResolution(value) {
    if (!RES_OPTIONS[value]) value = RES_DEFAULT;
    currentResolution = value;
    syncResUI();
  }

  function syncResUI() {
    var label = $("res-label");
    if (label) label.textContent = RES_OPTIONS[currentResolution] || RES_OPTIONS[RES_DEFAULT];
    var menu = $("res-menu");
    if (menu) {
      var items = menu.querySelectorAll("button[data-res]");
      for (var i = 0; i < items.length; i++) {
        items[i].classList.toggle("active", items[i].getAttribute("data-res") === currentResolution);
      }
    }
  }

  function toggleResMenu(force) {
    var menu = $("res-menu");
    if (!menu) return;
    var show = typeof force === "boolean" ? force : menu.classList.contains("hidden");
    menu.classList.toggle("hidden", !show);
    var btn = $("res-btn");
    if (btn) btn.setAttribute("aria-expanded", show ? "true" : "false");
  }

  function fillSettings(s) {
    lastSettings = s || {};
    setRam(ramMbFromSettings(s));
    setResolution((s && s.resolution) || RES_DEFAULT);
    var fs = $("fullscreen-toggle");
    if (fs) fs.checked = !!s.fullscreen;
    var cl = $("console-toggle");
    if (cl) cl.checked = s.console_live !== false;
    var ex = $("experimental-toggle");
    if (ex) ex.checked = !!s.experimental_runtime; var op = $("optimizations-toggle"); if (op) op.checked = !!s.experimental_optimizations;
    var gd = $("set-gamedir");
    if (gd) gd.value = s.game_dir || "";
    var ci = $("set-clientid");
    if (ci) ci.value = s.client_id || "";
    var area = $("jvm-args");
    var preset = s.jvm_preset || "46anarchy";
    if (area) {
      area.value = s.jvm_args || jvmPresetArgs(preset) || jvmPresetArgs("46anarchy");
    }
    var tabs = document.querySelectorAll("#jvm-presets button");
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].classList.toggle("active", tabs[i].getAttribute("data-preset") === preset);
    }
    syncPackFlavorUI();
    refreshJvmPresetLock(true);
  }

  function collectSettings() {
    var range = $("ram-range");
    var mb = range ? parseInt(range.value, 10) : 4096;
    var gb = Math.max(1, Math.round(mb / 1024));
    function val(id) {
      var el = $(id);
      return el ? el.value : "";
    }
    var fs = $("fullscreen-toggle"), cl = $("console-toggle"), ex = $("experimental-toggle"), op = $("optimizations-toggle");
    var preset = currentJvmPreset();
    var area = $("jvm-args");
    var args = area ? area.value : "";
    // Flags live on the Accueil flavor dropdown now; the Réglages toggles
    // are gone — preserve the last known values instead of defaulting off.
    var wantExp = ex ? ex.checked : (op ? op.checked : null);
    if (wantExp === null) {
      wantExp = !!(lastSettings.experimental_runtime || lastSettings.experimental_optimizations);
    }
    // `java_path` n'a plus de champ dans l'UI (carte runtime supprimée) :
    // la valeur du fichier est préservée telle quelle (le backend
    // l'honore en priorité quand elle est renseignée, vide = auto).
    // `java_runtime` est compat-only (dérivé de la flavor via
    // `java_runtime_for_flavor`) : l'UI ne le lit ni ne l'écrit.
    return {
      memory_min: "1G",
      memory_max: gb + "G",
      java_path: (lastSettings && lastSettings.java_path) || "",
      game_dir: val("set-gamedir"),
      client_id: val("set-clientid"),
      resolution: currentResolution || RES_DEFAULT,
      fullscreen: fs ? fs.checked : false,
      console_live: cl ? cl.checked : true,
      experimental_runtime: wantExp, experimental_optimizations: wantExp,
      jvm_preset: (args !== (jvmPresetArgs(preset) || "")) ? "custom" : preset,
      jvm_args: args
    };
  }

  /* ---------- java runtime (`check_java_status`, lecture seule) ----------
     La carte runtime de l'onglet Java est supprimée (plus d'installation
     manuelle ni de chemin personnalisé) comme la mini-carte d'accueil :
     seul le pied de page (version courte) et la modale Java (chemin)
     affichent le statut résolu, en lecture seule. Le runtime de la flavor
     est provisionné par le pipeline d'installation. */

  // Real probe fields only: path / major / is_java8 / warning. Loading and
  // error states stay honest, never a hardcoded "Java 8 x64".
  function renderJava(info) {
    var foot = $("java-footer-label");
    if (foot) foot.textContent = info.is_java8 ? "Java 8 x64" : (info.major ? ("Java " + info.major) : "—");
    var path = $("java-modal-path");
    if (path) path.textContent = info.path || "—";
  }

  function renderJavaError(err) {
    var foot = $("java-footer-label");
    if (foot) foot.textContent = "—";
  }

  function fetchJavaStatus() {
    if (!HAS_TAURI) return;
    invoke("check_java_status").then(renderJava, function () {
      invoke("resolve_java_info").then(renderJava, renderJavaError);
    });
  }

  /* ---------- carte JDK de la modale d'installation (lecture seule) ----------
     JDK livré avec les fichiers de la flavor SÉLECTIONNÉE — Temurin 8
     (Paladium) ou GraalVM 21 (expérimental). Aucun bouton : le runtime est
     provisionné par le pipeline d'installation. `get_java_runtimes` déjà
     récupéré est réutilisé ; actualisée à l'ouverture de la modale et à
     chaque changement de flavor. */
  var lastJavaRuntimes = null;

  function renderJdkInfoCard() {
    if (!$("jdk-info-card")) return;
    var exp = currentFlavor() === "experimental";
    var name = $("jdk-info-name");
    var status = $("jdk-info-status");
    if (name) name.textContent = exp ? "GraalVM 21" : "Temurin 8";
    if (!status) return;
    var info = lastJavaRuntimes ? (exp ? lastJavaRuntimes.graalvm21 : lastJavaRuntimes.temurin8) : null;
    var installed = !!info && (exp ? info.major === 21 : (info.major === 8 || !!info.is_java8));
    if (installed) {
      status.textContent = "Installé — Java " + info.major;
    } else {
      status.textContent = "Sera téléchargé avec le jeu";
    }
  }

  function fetchJavaRuntimes() {
    if (!HAS_TAURI) return;
    invoke("get_java_runtimes").then(function (r) {
      lastJavaRuntimes = r || null;
      renderJdkInfoCard();
    }, function () {
      lastJavaRuntimes = null;
      renderJdkInfoCard();
    });
  }

  // Avis ponctuels du démarrage (migration du verrou expérimental) :
  // le backend les met en file une seule fois, le frontend les affiche
  // en toast + journal puis les oublie (appel suivant vide).
  function fetchBootNotices() {
    if (!HAS_TAURI) return;
    invoke("get_boot_notices").then(function (lines) {
      (lines || []).forEach(function (l) {
        toast(l);
        try { log(l); } catch (e) {}
      });
    }, function () {});
  }

  function refreshAll() {
    if (!HAS_TAURI) { showNoBackendMode(); return; }
    invoke("get_session").then(function (s) {
      if (s) {
        settleBootAuth();
        bootAuthResolved = true;
        renderSession(s);
      } else if (bootAuthPending) {
        // Restore may still land: the boot poll owns the verdict, stay quiet.
        renderSession(null);
      } else {
        bootAuthResolved = true;
        renderSession(null);
      }
    }, function () {
      if (bootAuthPending) {
        renderSession(null);
      } else {
        bootAuthResolved = true;
        renderSession(null);
      }
    });
    invoke("get_manifest_status").then(renderStatus, function (err) {
      var sl = $("status-line");
      if (sl) sl.textContent = typeof err === "string" ? err : "Erreur.";
      var sub = $("client-sub");
      if (sub) sub.textContent = "—";
    });
    invoke("get_settings").then(function (s) {
      fillSettings(s);
    }, function (err) {
      toast(typeof err === "string" ? err : "Réglages injoignables.");
    });
    fetchBootNotices();
    // Différées après le premier affichage (audit du fan-out de démarrage) :
    // les sondes `java -version` (3 spawns : statut + 2 runtimes) et la RAM
    // système ne servent qu'à l'onglet Java / au pied de page, masqués au
    // boot — rien ne les attend pour peindre l'accueil. Gardées au premier
    // affichage : session/manifest/réglages (mémoire, instantanés), mods
    // (la ligne « N mods » de l'accueil vient du nombre renvoyé), serveur
    // (la carte serveur), notes de patch (la colonne). Le fan-out reste
    // concurrent (aucun `await` en chaîne) ; seul le lent interne —
    // notes de patch séquentielles — est parallélisé côté backend.
    setTimeout(function () {
      if (!HAS_TAURI) return;
      fetchJavaStatus();
      fetchJavaRuntimes();
      fetchSystemMemory();
    }, 1500);
    refreshMods(true);
    refreshAccounts();
    fetchServerStatus(true);
    fetchPatchNotes(true);
    fetchLauncherVersion();
    fetchBuildInfo();
  }

  function autoRefreshManifest() {
    if (!HAS_TAURI) return;
    invoke("refresh_manifest").then(function (st) {
      renderStatus(st);
    }, function () {});
  }

  /* ---------- actions (real commands only) ---------- */

  function requestPlay() {
    if (!requireBackend()) return;
    if (!sessionCached) {
      toast("Connectez-vous pour jouer.");
      resetLoginModal();
      openLoginModal();
      return;
    }
    if (playBusy || installBusy || gameRunning) return;
    if (installNeeded()) {
      // Le bouton principal affiche INSTALLER : on installe sans lancer.
      // JOUER (doPlay) reste la seule entrée de lancement ensuite.
      openLokiModal("install");
      return;
    }
    doPlay();
  }

  function doPlay() {
    if (!requireBackend()) return;
    if (!sessionCached) {
      toast("Connectez-vous pour jouer.");
      resetLoginModal();
      openLoginModal();
      return;
    }
    if (playBusy || installBusy || gameRunning) return;
    playBusy = true;
    setPlayBusy(true, null, "Lancement…");
    invoke("play").then(function (res) {
      playBusy = false;
      setPlayBusy(false);
      hideHeroProgress();
      log("Lancement : " + res.command);
      (res.warnings || []).forEach(function (w) { log("Note : " + w); });
      // En jeu edge-triggered au succès du lancement, edge-cleared à la détection de fermeture.
      gameRunning = true;
      gamePid = null;
      renderGameRunningButton();
      toast("Jeu lancé.");
      refreshAll();
    }, function (err) {
      playBusy = false;
      setPlayBusy(false);
      hideHeroProgress();
      if (isLoginRequired(err)) {
        toast("Connexion Microsoft requise pour jouer.");
        resetLoginModal();
        openLoginModal();
        return;
      }
      var pmsg = typeof err === "string" ? err : "Échec du lancement.";
      toast(pmsg);
      var pdet = $("install-detail");
      if (pdet) pdet.textContent = pmsg;
      log("Échec du lancement : " + err);
    });
  }

  function setCancelVisible(on) {
    var btn = $("btn-cancel-install");
    if (btn) btn.classList.toggle("hidden", !on);
  }

  function doCancelInstall() {
    if (!requireBackend()) return;
    invoke("cancel_install").then(function () {
      log("Annulation demandée…");
    }, function (err) {
      toast(typeof err === "string" ? err : "Annulation impossible.");
    });
  }

  function doInstall(includeLoki) {
    if (!requireBackend()) return;
    if (installBusy || playBusy) return;
    installBusy = true;
    var btn = $("btn-install");
    if (btn) btn.disabled = true;
    playBusy = false;
    setPlayBusy(true, 0);
    setCancelVisible(true);
    log(includeLoki ? "Installation… (Loki : inclus, cheat optionnel)" : "Installation… (Loki : non installé)");
    showInstallModal();
    invoke("start_install", { include_loki: !!includeLoki }).then(function (res) {
      installBusy = false;
      if (btn) btn.disabled = false;
      setPlayBusy(false);
      setCancelVisible(false);
      hideInstallModal();
      renderProgress({ fraction: 1, detail: "Terminé : " + res.downloaded_files + " téléchargés, " + res.skipped_files + " à jour." });
      log("Terminé : " + res.downloaded_files + " téléchargés, " + res.skipped_files + " à jour (" + res.total_files + " fichiers).");
      if (includeLoki) log("Loki (cheat optionnel) : installé dans mods/loki.jar.");
      toast("Jeu installé et vérifié.");
      installModalDone("Terminé : " + res.downloaded_files + " téléchargés, " + res.skipped_files + " à jour.");
      refreshAll();
      hideHeroProgress();
    }, function (err) {
      installBusy = false;
      if (btn) btn.disabled = false;
      setPlayBusy(false);
      setCancelVisible(false);
      hideHeroProgress();
      hideInstallModal();
      if (isLoginRequired(err)) {
        toast("Connexion Microsoft requise pour jouer.");
        resetLoginModal();
        openLoginModal();
        return;
      }
      var msg = typeof err === "string" ? err : "Échec de l'installation.";
      toast(msg);
      var det = $("install-detail");
      if (det) det.textContent = msg;
      log("Échec de l'installation : " + err);
    });
  }

  /* ---------- wiring ---------- */

  // Garde anti-double-armorçage : si l'init s'exécutait deux fois, les
  // `setInterval` s'empileraient (sondages en double + réveils en double).
  var booted = false;
  document.addEventListener("DOMContentLoaded", function () {
    if (booted) return;
    booted = true;
    var items = document.querySelectorAll(".nav-item");
    for (var i = 0; i < items.length; i++) {
      (function (t) {
        t.addEventListener("click", function () { switchTab(t.getAttribute("data-tab")); });
      })(items[i]);
    }

    on("btn-play", "click", requestPlay);
    on("btn-install", "click", function () {
      if (!requireBackend()) return;
      if (installBusy || playBusy) return;
      openLokiModal("install");
    });
    on("pack-flavor-btn", "click", function (ev) {
      ev.stopPropagation();
      togglePackFlavorMenu();
    });
    on("pack-flavor-edit", "click", function () {
      if (!requireBackend()) return;
      openLokiModal("install");
    });
    document.addEventListener("click", function () { togglePackFlavorMenu(false); });
    (function () {
      var menu = $("pack-flavor-menu");
      if (!menu) return;
      var items = menu.querySelectorAll("button[data-flavor]");
      for (var i = 0; i < items.length; i++) {
        (function (b) {
          b.addEventListener("click", function (ev) {
            ev.stopPropagation();
            togglePackFlavorMenu(false);
            if (!requireBackend()) return;
            setPackFlavor(b.getAttribute("data-flavor"));
          });
        })(items[i]);
      }
    })();
    on("btn-install-modal-cancel", "click", function () {
      doCancelInstall();
    });
    on("btn-loki-install", "click", confirmLokiModal);
    on("btn-loki-cancel", "click", cancelLokiModal);
    on("btn-verify", "click", function () {
      if (!requireBackend()) return;
      invoke("verify_files").then(function (r) {
        var msg = r.stale === 0
          ? "Vérifié : " + r.total + " fichiers à jour."
          : "Vérifié : " + r.stale + " fichiers sur " + r.total + " à télécharger.";
        var det = $("install-detail");
        if (det) det.textContent = msg;
        log(msg);
      }, function (err) { toast(typeof err === "string" ? err : "Échec de la vérification."); });
    });
  function toggleLaunchMenu(force) {
    var menu = $("launch-menu");
    if (!menu) return;
    var show = typeof force === "boolean" ? force : menu.classList.contains("hidden");
    menu.classList.toggle("hidden", !show);
  }

  function doRefreshManifest() {
    if (!requireBackend()) return;
    invoke("refresh_manifest").then(function (st) {
      renderStatus(st);
      log("Manifeste " + (st.version || "?") + " récupéré.");
    }, function (err) { toast(typeof err === "string" ? err : "CDN injoignable."); });
  }

  function doOpenFolder() {
    if (!requireBackend()) return;
    invoke("open_game_folder").then(function (p) {
      toast("Dossier ouvert : " + p);
    }, function (err) { toast(typeof err === "string" ? err : "Ouverture impossible."); });
  }

    on("btn-launch-menu", "click", function (ev) {
      ev.stopPropagation();
      toggleLaunchMenu();
    });
    document.addEventListener("click", function () { toggleLaunchMenu(false); });
    on("menu-manifest", "click", function () {
      toggleLaunchMenu(false);
      doRefreshManifest();
    });
    on("menu-folder", "click", function () {
      toggleLaunchMenu(false);
      doOpenFolder();
    });
    on("menu-uninstall", "click", function () {
      toggleLaunchMenu(false);
      if (!requireBackend()) return;
      if (installBusy || playBusy) {
        toast("Terminez l'opération en cours d'abord.");
        return;
      }
      var m = $("uninstall-modal");
      if (m) m.classList.remove("hidden");
      var confirm = $("btn-uninstall-confirm");
      if (confirm) confirm.focus();
    });
    on("btn-uninstall-cancel", "click", function () {
      var m = $("uninstall-modal");
      if (m) m.classList.add("hidden");
    });
    on("btn-uninstall-confirm", "click", function () {
      var m = $("uninstall-modal");
      if (m) m.classList.add("hidden");
      if (!requireBackend()) return;
      invoke("uninstall_game").then(function (r) {
        toast("Jeu désinstallé (" + r.removed_files + " fichiers).");
        log("Désinstallation : " + r.removed_files + " fichiers, " + r.removed_dirs + " dossiers.");
        refreshAll();
      }, function (err) { toast(typeof err === "string" ? err : "Désinstallation impossible."); });
    });

    (function () {
      var m = $("loki-modal");
      if (m) {
        m.addEventListener("click", function (e) {
          if (e.target === m) cancelLokiModal();
        });
      }
      document.addEventListener("keydown", function (e) {
        if ((e.key === "Escape" || e.key === "Esc") && m && !m.classList.contains("hidden")) {
          cancelLokiModal();
        }
      });
    })();

    var mtabs = document.querySelectorAll("#mods-filters .filter-tab");
    for (var f = 0; f < mtabs.length; f++) {
      (function (t) {
        t.addEventListener("click", function () {
          for (var j = 0; j < mtabs.length; j++) mtabs[j].classList.remove("active");
          t.classList.add("active");
          modsFilter = t.getAttribute("data-filter");
          applyModsFilter();
        });
      })(mtabs[f]);
    }
    on("mods-search", "input", applyModsFilter);
    on("btn-open-mods", "click", function () {
      if (!requireBackend()) return;
      invoke("open_mods_folder").then(function (p) {
        toast("Dossier ouvert : " + p);
      }, function (err) { toast(typeof err === "string" ? err : "Ouverture impossible."); });
    });

    on("ram-range", "input", function (e) { setRam(parseInt(e.target.value, 10)); });
    on("ram-range", "change", function () { persistSettingsNow(); });
    var rbtns = document.querySelectorAll("#ram-presets .preset-btn");
    for (var r = 0; r < rbtns.length; r++) {
      (function (b) {
        b.addEventListener("click", function () { setRam(parseInt(b.getAttribute("data-ram"), 10)); persistSettingsNow(); });
      })(rbtns[r]);
    }
    var jtabs = document.querySelectorAll("#jvm-presets button");
    for (var jt = 0; jt < jtabs.length; jt++) {
      (function (b) {
        b.addEventListener("click", function () {
          if (b.disabled) return;
          applyJvmPreset(b.getAttribute("data-preset"));
          persistSettingsNow();
        });
      })(jtabs[jt]);
    }
    on("jvm-args", "input", function () { syncJvmTabsToText(); scheduleSettingsSave(); });
    on("res-btn", "click", function (ev) {
      ev.stopPropagation();
      toggleResMenu();
    });
    document.addEventListener("click", function () { toggleResMenu(false); });
    (function () {
      var menu = $("res-menu");
      if (!menu) return;
      var items = menu.querySelectorAll("button[data-res]");
      for (var i = 0; i < items.length; i++) {
        (function (b) {
          b.addEventListener("click", function (ev) {
            ev.stopPropagation();
            toggleResMenu(false);
            setResolution(b.getAttribute("data-res"));
            persistSettingsNow();
          });
        })(items[i]);
      }
    })();
    on("fullscreen-toggle", "change", function () { persistSettingsNow(); });
    on("console-toggle", "change", function () { persistSettingsNow(); });
    on("set-gamedir", "input", scheduleSettingsSave);
    on("set-clientid", "input", scheduleSettingsSave);
    on("btn-reset-all", "click", function () {
      if (!requireBackend()) return;
      setRam(4096);
      var gd = $("set-gamedir");
      if (gd) gd.value = "";
      setResolution(RES_DEFAULT);
      var fs = $("fullscreen-toggle");
      if (fs) fs.checked = false;
      var cl = $("console-toggle");
      if (cl) cl.checked = true;
      var wantExpReset = currentFlavor() === "experimental";
      applyJvmPreset(wantExpReset ? "graal-zgc" : JVM_ARGS_DEFAULT);
      persistSettingsNow(function () { toast("Valeurs par défaut restaurées."); });
    });

    on("btn-microsoft", "click", startLogin);
    on("btn-cancel-login", "click", cancelLogin);
    on("btn-cancel-login-device", "click", cancelLogin);
    on("user-chip", "click", function (ev) {
      ev.stopPropagation();
      toggleAccountMenu();
    });
    document.addEventListener("click", function () { toggleAccountMenu(false); });
    on("btn-login-accounts", "click", function () {
      resetLoginModal();
      startLogin();
    });

    on("btn-cancel-install", "click", doCancelInstall);
    on("btn-clear-gamelog", "click", clearGameLog);
    on("btn-clear-log", "click", function () {
      installLogQueue.length = 0;
      var el = $("install-log");
      if (el) {
        el.textContent = LOG_PLACEHOLDER;
        el.scrollTop = 0;
        installLogCount = 0;
      }
    });
    on("btn-open-folder", "click", function () {
      if (!requireBackend()) return;
      invoke("open_game_folder").then(function (p) {
        toast("Dossier ouvert : " + p);
      }, function (err) { toast(typeof err === "string" ? err : "Ouverture impossible."); });
    });

    on("link-site", "click", function (e) {
      if (HAS_TAURI) {
        e.preventDefault();
        openUrl("https://46anarchy.fr/");
      }
    });
    on("link-discord-top", "click", function (e) {
      if (HAS_TAURI) {
        e.preventDefault();
        openUrl(DISCORD_URL);
      }
    });
    on("btn-changelog", "click", function () {
      patchExpanded = !patchExpanded;
      var b = $("btn-changelog");
      if (b) b.textContent = patchExpanded ? "Réduire" : "Historique complet →";
      lastPatchNotesSig = null;
      renderPatchNotes(lastPatchNotesCache);
    });

    on("btn-retry-login", "click", function () {
      resetLoginModal();
      startLogin();
    });
    on("btn-copy-code", "click", function () {
      var code = $("user-code");
      if (code) copyText(code.textContent.trim(), "Code copié.");
    });
    function openMs() {
      var link = $("verification-link");
      if (link) openUrl(link.href);
    }
    on("btn-open-ms", "click", openMs);
    on("verification-link", "click", function (e) {
      if (HAS_TAURI) {
        e.preventDefault();
        openMs();
      }
    });

    if (tauriEvent && tauriEvent.listen) {
      tauriEvent.listen("install-progress", function (e) { renderProgress(e.payload); });
      tauriEvent.listen("console-line", function (e) { glog(e.payload); });
      tauriEvent.listen("game-outcome", function (e) { handleGameOutcome(e.payload); });
      tauriEvent.listen("session-changed", function (e) { settleBootAuth(); applySessionUpdate(e.payload); });
    }

    on("btn-dismiss-demo", "click", function () {
      var b = $("demo-banner");
      if (b) b.classList.add("hidden");
    });
    if (!HAS_TAURI) {
      showNoBackendMode();
      switchTab("home");
      return;
    }
    switchTab("home");
    refreshAll();
    startBootAuthPoll();
    autoRefreshManifest();
    setInterval(function () {
      if (HAS_TAURI && !document.hidden) fetchServerStatus();
      pollSession();
    }, STATUS_INTERVAL_MS);
    setInterval(pollGameStatus, GAME_STATUS_INTERVAL_MS);
    setInterval(pollInstallProgress, INSTALL_POLL_MS);
    setInterval(pollConsoleTail, 1000);
    document.addEventListener("visibilitychange", function () {
      if (!document.hidden && HAS_TAURI) {
        fetchServerStatus();
        fetchPatchNotes();
      }
    });
  });
})();
