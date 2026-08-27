// Hydra WebUI - Dashboard

const API_KEY = localStorage.getItem("hydra_api_key") || "";
let _provenance = null;
const POLL_INTERVAL = 1000;

let _sessionBaselineUl = null;
let _sessionBaselineDl = null;

// The dot answers "can a peer open a connection to us", which is measured by a
// probe (see reachability.go). It used to be `peers > 0`, which answers a
// different question entirely: every peer we dialled ourselves counts, so a node
// nobody could reach looked perfectly healthy and stayed leech-only.
function _paintHealthDot(id, reach, port, peers, warnings, engine) {
    const dot = document.getElementById(id);
    if (!dot) return;
    const st = (reach && reach.state) || "unknown";
    const cls = st === "reachable" ? "ok" : (st === "unreachable" ? "error" : "warn");
    dot.className = "health-dot " + cls;
    const head = st === "reachable"
        ? t("Peers can reach you on port {port}", { port: port })
        : (st === "unreachable"
            ? t("Nobody can reach you on port {port}", { port: port })
            : t("Reachability on port {port} not established yet", { port: port }));
    let lines = [engine + ": " + head];
    if (reach && reach.detail) lines.push(reach.detail);
    lines.push(t("{n} peers connected", { n: peers }));
    if (warnings && warnings.length) lines = [...warnings, "", ...lines];
    const row = dot.closest(".health-row") || dot;
    row.title = lines.join("\n");
}

async function fetchPortForward() {
    try {
        const d = await api("/api/port-forward");
        const dot = document.getElementById("health-dot");

        // Determine health class
        let cls = "ok";
        let warnings = [];

        if (!d.listen_healthy) {
            cls = "error";
            // Find stale sockets
            const stale = [...(d.race_sockets || []), ...(d.hoard_sockets || [])].filter(s => s.stale);
            for (const s of stale) {
                warnings.push("⚠ " + t("{addr}, stale socket (bound {iface}, interface recreated)", { addr: s.ip + ":" + s.port, iface: s.bound_interface }));
            }
        } else if (!d.all_connectable) {
            cls = d.race_connectable || d.hoard_connectable ? "warn" : "error";
        }

        if (dot) dot.className = "health-dot " + cls;
        _ipv6Wanted = !!d.ipv6_wanted;
        // The header address is written by the per-engine measurement now (see
        // updateNetPoly). This one is the whole process's default route, which
        // is not any engine's exit as soon as one is bound to a tunnel.
    } catch {}
}

// Whether IPv6 was asked for in the settings. Remembered so the manual refresh,
// which only knows the addresses, still renders the same thing as the poll.
let _ipv6Wanted = false;

// One node's exit address as {html, title}, one line per family. Shared by the
// header and the agents table so a remote node reads exactly like this one.
function _exitIPMarkup(v4, v6, v6Wanted) {
    const lines = [], tips = [];
    if (v4) { lines.push(`<span class="ip-line">${esc(incoExitIP(v4))}</span>`); tips.push(incoExitIP(v4)); }
    if (v6) {
        lines.push(`<span class="ip-line">${esc(incoExitIP(v6))}</span>`);
        tips.push(incoExitIP(v6));
    } else if (v6Wanted) {
        // Asked for, not available: saying so beats a lone v4 that passes for a
        // working dual stack.
        lines.push(`<span class="ip-line net-warn">${esc(t("IPv6 unavailable"))}</span>`);
        tips.push(t("IPv6 is enabled in the settings, but this host has no IPv6 address, so nothing is listening or announced on it."));
        return { html: lines.join(""), title: tips.join(" / ") };
    }
    // The tooltip holds every address, masked the same way, so a clipped line
    // is never the only copy and incognito stays incognito on hover.
    return { html: lines.join("") || "\u2014", title: tips.join(" / ") };
}

// The single place the header address is written. Two writers used to race
// here, and the one behind the click knew nothing about IPv6, so refreshing by
// hand dropped the second address.
function _renderHeaderIP(v4, v6) {
    const el = document.getElementById("header-exit-ip");
    if (!el || !v4) return;
    const m = _exitIPMarkup(v4, v6, _ipv6Wanted);
    el.innerHTML = m.html;
    el.title = m.title;
}

// Filled by updateNetPoly (below). Declared here because fetchPublicIp, which
// runs first, has to know whether the per-engine measurement owns the header.
let _netEngines = [];

let ipScrambleTimer = null;
// Slot-machine scramble on the exit-IP text while a manual refresh is in flight.
function startIpScramble(el) {
    if (!el) return;
    stopIpScramble();
    const chars = "0123456789.";
    el.style.fontFamily = "ui-monospace, monospace";
    ipScrambleTimer = setInterval(() => {
        let out = "";
        for (let i = 0; i < 11; i++) out += chars[(Math.random() * chars.length) | 0];
        el.textContent = out;
    }, 45);
}
function stopIpScramble() {
    if (ipScrambleTimer) { clearInterval(ipScrambleTimer); ipScrambleTimer = null; }
    const el = document.getElementById("header-exit-ip");
    if (el) el.style.fontFamily = "";
}

async function fetchPublicIp(force) {
    const hdrScr = document.getElementById("header-exit-ip");
    const prevHdr = hdrScr ? hdrScr.textContent : "";
    let ipUpdated = false;
    const rbtn = document.querySelector(".ip-refresh-btn");
    if (force) { startIpScramble(hdrScr); if (rbtn) rbtn.classList.add("spin"); }
    try {
        const d = await api("/api/public-ip" + (force ? "?refresh=1" : ""));
        if (d.ip && d.ip !== "unknown") {
            if (force) stopIpScramble();
            // This is the PROCESS's exit -- its default route -- which is not
            // any engine's as soon as one is bound to a tunnel. It only writes
            // the header while the per-engine measurement has nothing to say.
            if (!_netEngines.some(e => e.exit_ip)) _renderHeaderIP(d.ip, d.ip_v6);
            ipUpdated = true;
            // Leak detection: any IP that is NOT our home WAN means we're
            // properly behind a tunnel. Avoids hardcoding takehost IPs which
            // change when DPI bans hit and we rotate to a new VPS IP.
            // Direct mode (prod direct depuis 2026-08-01) : l'IP maison EST
            // l'exit voulu, plus une fuite. Le dot suit le port-forward/listen.
            const el = document.getElementById("tunnel-info");
            const dot = document.getElementById("health-dot");
            if (el) el.classList.remove("tunnel-leak");
            const _leakRow = el && el.querySelector(".tunnel-leak-row");
            if (_leakRow) _leakRow.remove();
            if (dot) { dot.style.background = ""; dot.title = t("Exit IP: {ip}", { ip: d.ip + (d.ip_v6 ? " / " + d.ip_v6 : "") }); }
            const exitEl = document.getElementById("proxy-exit-ip");
            if (exitEl) exitEl.textContent = incoExitIP(d.ip_v6 || d.ip);
        }
    } catch {}
    finally {
        if (force) {
            stopIpScramble();
            if (rbtn) rbtn.classList.remove("spin");
            if (!ipUpdated && hdrScr) hdrScr.textContent = prevHdr;
            // A manual refresh means "measure again", which for the engines
            // happens on the node: ask for it, then read the result once it has
            // had time to land rather than holding the click open for it.
            try { await api("/api/network/engines?refresh=1"); } catch (_) {}
            setTimeout(updateNetPoly, 4000);
        }
    }
}

// ─── Utilities ──────────────────────────────────────────

function _unitSize() { return localStorage.getItem("hydra_unit_size") || "binary"; }
function _unitSpeed() { return localStorage.getItem("hydra_unit_speed") || "bytes"; }
function setUnitPref(kind, value) {
    localStorage.setItem(kind === "size" ? "hydra_unit_size" : "hydra_unit_speed", value);
    try { updateOverview(); } catch (_) {}
    try { if (typeof _scheduleHoardRender === "function") _scheduleHoardRender(); } catch (_) {}
    try { if (typeof updateRaceTorrents === "function") updateRaceTorrents(); } catch (_) {}
}

function formatBytes(bytes) {
    if (bytes === 0) return "0 B";
    const decimal = _unitSize() === "decimal";
    const k = decimal ? 1000 : 1024;
    const sizes = decimal
        ? ["B", "KB", "MB", "GB", "TB", "PB"]
        : ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), sizes.length - 1);
    return (bytes / Math.pow(k, i)).toPrecision(4) + " " + sizes[i];
}

function formatSpeed(bytesPerSec) {
    return _unitSpeed() === "bits" ? formatGbps(bytesPerSec) : formatBytes(bytesPerSec) + "/s";
}

// Switching language reloads the page on purpose: re-translating a DOM that is
// already translated cannot work, since the English key it was built from is
// gone by then. Same reasoning as the first-run picker.
function setUiLang(code) {
    I18N.setLang(code).then(() => location.reload());
}

function _languageCardHTML() {
    const cur = I18N.current();
    const opts = I18N.languages.map(l =>
        `<option value="${l.code}"${l.code === cur ? " selected" : ""}>${l.label}</option>`
    ).join("");
    return `<div class="settings-section" style="margin-bottom:18px">
        <div class="settings-section-title">${t("Language")}</div>
        <div class="settings-row">
            <div class="sr-label"><span class="sr-key">${t("Interface language")}</span><span class="sr-desc">${t("Language of the WebUI. Stored in this browser, so each browser can differ. Changing it reloads the page.")}</span></div>
            <div class="sr-field"><select class="sr-input" onchange="setUiLang(this.value)">${opts}</select></div>
        </div>
    </div>`;
}

function _unitsCardHTML() {
    const sz = _unitSize(), sp = _unitSpeed();
    const opt = (v, cur, label) => `<option value="${v}"${v === cur ? " selected" : ""}>${label}</option>`;
    return `<div class="settings-section" style="margin-bottom:18px">
        <div class="settings-section-title">${t("Display units")}</div>
        <div class="settings-row">
            <div class="sr-label"><span class="sr-key">${t("Sizes")}</span><span class="sr-desc">${t("Binary (MiB, \u00d71024) or decimal (MB, \u00d71000).")}</span></div>
            <div class="sr-field"><select class="sr-input" onchange="setUnitPref('size', this.value)">${opt("binary", sz, t("Binary \u2014 KiB / MiB / GiB"))}${opt("decimal", sz, t("Decimal \u2014 KB / MB / GB"))}</select></div>
        </div>
        <div class="settings-row">
            <div class="sr-label"><span class="sr-key">${t("Speeds")}</span><span class="sr-desc">${t("Bytes per second (MiB/s) or bits per second (Mbps).")}</span></div>
            <div class="sr-field"><select class="sr-input" onchange="setUnitPref('speed', this.value)">${opt("bytes", sp, t("Bytes/s \u2014 KiB/s, MiB/s"))}${opt("bits", sp, t("Bits/s \u2014 Mbps, Gbps"))}</select></div>
        </div>
    </div>`;
}

function _interfacesCardHTML(list) {
    const rows = (list && list.length)
        ? list.map((i) => `<div class="settings-row"><div class="sr-label"><span class="sr-key">${esc(i.name)}</span><span class="sr-desc">${i.up ? t("up") : t("down")}</span></div><div class="sr-field"><code class="sr-readonly">${esc(i.ip)}</code></div></div>`).join("")
        : `<div class="settings-row"><div class="sr-desc">${t("No non-loopback interfaces detected.")}</div></div>`;
    return `<div class="settings-section" style="margin-bottom:18px">
        <div class="settings-section-title">${t("Network interfaces")}</div>
        <div class="sr-desc" style="padding:0 0 8px">${t("Detected on this host. To pin an engine to one, set <code>bind_interface</code> to its <b>name</b> (survives VPN IP changes) under [race]/[hoard], or <code>listen_interfaces</code> to <code>ip:port</code>.")}</div>
        ${rows}
    </div>`;
}

// Throughput in bits/s (network convention, decimal), Option 2: VOLUMES in iB, RATES in Gbps.
function formatGbps(bytesPerSec) {
    const bits = (bytesPerSec || 0) * 8;
    if (bits >= 1e9) return (bits / 1e9).toFixed(2) + " Gbps";
    if (bits >= 1e6) return Math.round(bits / 1e6) + " Mbps";
    if (bits >= 1e3) return Math.round(bits / 1e3) + " Kbps";
    return Math.round(bits) + " bps";
}

function formatCount(n) {
    if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
    if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
    return n.toString();
}

function formatUptime(seconds) {
    const d = Math.floor(seconds / 86400);
    const h = Math.floor((seconds % 86400) / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    if (d > 0) return `${d}d ${h}h`;
    if (h > 0) return `${h}h ${m}m`;
    return `${m}m`;
}

let _setupOpen = false;
// Ecran de premier lancement : aucun compte admin n existe encore, l humain
// choisit son mot de passe. Hydra n en genere plus aucun -> rien a perdre si le
// demarrage echoue en route. POST /api/setup renvoie l API key comme /api/login.
function promptFirstRunSetup(networkStorage) {
    if (_setupOpen) return;
    _setupOpen = true;
    const warn = networkStorage ? `<p class="modal-desc" style="color:var(--accent-orange)">
        Config on network storage detected (${esc(networkStorage)}). Hydra can handle this,
        but the database cannot use its safest journal mode there: if the share drops
        mid-write it can be corrupted. Keep backups of your data_dir, or move data_dir
        to a local disk (your downloads can stay on the share).</p>` : "";
    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    ov.innerHTML = `<div class="modal-box">
        <h3>${t("Welcome to Hydra")}</h3>
        <label class="setup-lang-row">${t("Language")}
            <select id="setup-lang">${I18N.languages.map(l =>
                `<option value="${l.code}"${l.code === I18N.current() ? " selected" : ""}>${l.label}</option>`
            ).join("")}</select>
        </label>
        <p class="modal-desc" id="setup-msg">${t("Create your admin account to get started.")}</p>
        ${warn}
        <input type="text" id="setup-user" placeholder="${t("Username")}" autocomplete="username" value="admin" style="width:100%;margin-bottom:8px">
        <input type="password" id="setup-pass" placeholder="${t("Password (min 8 characters)")}" autocomplete="new-password" style="width:100%;margin-bottom:8px">
        <input type="password" id="setup-pass2" placeholder="${t("Confirm password")}" autocomplete="new-password" style="width:100%">
        <div class="modal-actions">
            <button class="btn-primary" id="setup-go">${t("Create account")}</button>
        </div>
    </div>`;
    document.body.appendChild(ov);
    // Reloading is the honest way to switch: re-translating an already
    // translated DOM cannot work, the English key is gone by then.
    const langSel = ov.querySelector("#setup-lang");
    if (langSel) langSel.addEventListener("change", () => {
        I18N.setLang(langSel.value).then(() => location.reload());
    });
    const user = ov.querySelector("#setup-user");
    const pass = ov.querySelector("#setup-pass");
    const pass2 = ov.querySelector("#setup-pass2");
    const msg = ov.querySelector("#setup-msg");
    pass.focus();
    const fail = (t) => { msg.textContent = t; msg.style.color = "var(--accent-red)"; };
    const go = async () => {
        if (pass.value.length < 8) return fail(t("Password too short (min 8 characters)."));
        if (pass.value !== pass2.value) return fail(t("The two passwords do not match."));
        try {
            const res = await fetch("/api/setup", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ username: user.value, password: pass.value }),
            });
            const d = await res.json().catch(() => ({}));
            if (!res.ok) return fail(d.error || t("Setup failed ({status})", { status: res.status }));
            localStorage.setItem("hydra_api_key", d.api_key);
            location.reload();
        } catch (err) {
            fail(t("Network error: {msg}", { msg: err.message }));
        }
    };
    ov.querySelector("#setup-go").addEventListener("click", go);
    [user, pass, pass2].forEach(el => el.addEventListener("keydown", e => { if (e.key === "Enter") go(); }));
}

let _loginOpen = false;
// Modale de login user/mot de passe. POST /api/login -> renvoie l API key,
// stockee en localStorage (la WebUI l utilise ensuite pour X-Api-Key).
// Une install neuve n a pas encore de compte -> on bascule sur l ecran de setup.
async function promptLogin(reason) {
    if (_loginOpen || _setupOpen) return;
    try {
        const st = await (await fetch("/api/setup")).json();
        if (st && st.needs_setup) return promptFirstRunSetup(st.network_storage);
    } catch (_) { /* pas joignable: on retombe sur le login normal */ }
    if (_loginOpen) return;
    _loginOpen = true;
    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    ov.innerHTML = `<div class="modal-box">
        <h3>Hydra login</h3>
        <p class="modal-desc" id="login-msg">${esc(reason || "Sign in to continue.")}</p>
        <input type="text" id="login-user" placeholder="Username" autocomplete="username" value="admin" style="width:100%;margin-bottom:8px">
        <input type="password" id="login-pass" placeholder="Password" autocomplete="current-password" style="width:100%">
        <div class="modal-actions">
            <button class="btn-primary" id="login-go">Sign in</button>
        </div>
    </div>`;
    document.body.appendChild(ov);
    const user = ov.querySelector("#login-user");
    const pass = ov.querySelector("#login-pass");
    const msg = ov.querySelector("#login-msg");
    pass.focus();
    const go = async () => {
        try {
            const res = await fetch("/api/login", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ username: user.value, password: pass.value }),
            });
            const d = await res.json().catch(() => ({}));
            if (!res.ok) {
                msg.textContent = d.error || t("Login failed ({status})", { status: res.status });
                msg.style.color = "var(--accent-red)";
                return;
            }
            localStorage.setItem("hydra_api_key", d.api_key);
            location.reload();
        } catch (err) {
            msg.textContent = "Network error: " + err.message;
            msg.style.color = "var(--accent-red)";
        }
    };
    ov.querySelector("#login-go").addEventListener("click", go);
    [user, pass].forEach(el => el.addEventListener("keydown", e => { if (e.key === "Enter") go(); }));
}

let _apiKeyPromptOpen = false;
// Modale de saisie de la clef API. La WebUI lit sa clef dans localStorage
// ("hydra_api_key") ; sans champ de saisie, un navigateur neuf est bloque.
function promptApiKey(reason) {
    if (_apiKeyPromptOpen) return;
    _apiKeyPromptOpen = true;
    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    ov.innerHTML = `<div class="modal-box">
        <h3>API key</h3>
        <p class="modal-desc">${esc(reason || "Enter the daemon API key (config: [daemon] api_key).")}</p>
        <input type="text" id="apikey-input" placeholder="X-Api-Key" value="${esc(API_KEY || "")}" autocomplete="off" spellcheck="false" style="width:100%">
        <div class="modal-actions">
            <button class="btn-primary" id="apikey-save">Save &amp; reload</button>
        </div>
    </div>`;
    document.body.appendChild(ov);
    const inp = ov.querySelector("#apikey-input");
    inp.focus();
    inp.select();
    const save = () => {
        const v = inp.value.trim();
        if (!v) return;
        localStorage.setItem("hydra_api_key", v);
        location.reload();
    };
    ov.querySelector("#apikey-save").addEventListener("click", save);
    inp.addEventListener("keydown", e => { if (e.key === "Enter") save(); });
}

// ── qBittorrent import wizard ─────────────────────────────────────────────
// Onboarding: connect to a running qBittorrent WebUI and seed its library into
// Hydra (hoard). 3 steps: credentials → preview → live progress (SSE).
// Opened two ways, and the exit button is not the same thing in each: from the
// first-run prompt it is "Skip", which also means "stop asking me"; from
// Settings the user went looking for the wizard, so it is a plain "Cancel" that
// must not silently dismiss the onboarding.
let _importOpen = false;
function importWizard(opts) {
    const fromSettings = !!(opts && opts.fromSettings);
    if (_importOpen) return;
    _importOpen = true;
    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    const box = document.createElement("div");
    box.className = "modal-box";
    box.style.maxWidth = "560px";
    ov.appendChild(box);
    document.body.appendChild(ov);
    const close = () => { ov.remove(); _importOpen = false; };

    function stepSource() {
        box.innerHTML = `<h3>Import an existing library</h3>
            <p class="modal-desc">Hydra seeds the data already on your disk, completed torrents skip the hash-check, so nothing is re-downloaded.</p>
            <div style="display:flex;gap:8px;margin:12px 0">
                <button class="btn-primary" id="src-qbit" style="flex:1">qBittorrent</button>
                <button class="btn-primary" id="src-tr" style="flex:1">Transmission</button>
            </div>
            <div class="modal-actions"><button id="src-skip" class="btn-small">${fromSettings ? t("Cancel") : t("Skip")}</button></div>`;
        box.querySelector("#src-skip").onclick = () => {
            if (!fromSettings) localStorage.setItem("hydra_import_dismissed", "1");
            close();
        };
        box.querySelector("#src-qbit").onclick = () => stepCreds();
        box.querySelector("#src-tr").onclick = () => stepTransmission();
    }

    // Transmission cannot hand over a .torrent (its RPC only exposes the path),
    // so the import reads its config folder. Which also means it works when
    // Transmission is already stopped, usually the case during a migration.
    function stepTransmission(prefill) {
        box.innerHTML = `<h3>${t("Import from Transmission")}</h3>
            <p class="modal-desc">${t("Hydra reads Transmission's <b>config folder</b>, the one holding <code>torrents/</code> and <code>resume/</code>. Transmission does not need to be running.")}</p>
            <input type="text" id="tr-dir" placeholder="/config/transmission or ~/.config/transmission-daemon" style="width:100%;margin-bottom:8px" value="${esc(prefill && prefill.dir || "")}">
            <p class="modal-desc" style="margin:6px 0">${t("Not visible from Hydra? Upload a zip of that folder instead:")}</p>
            <input type="file" id="tr-zip" accept=".zip" style="width:100%;margin-bottom:8px">
            <label style="display:block;margin-bottom:4px"><input type="checkbox" id="tr-cats" checked> ${t("Create one category per destination folder")}</label>
            <label style="display:block"><input type="checkbox" id="tr-labels" checked> ${t("Import labels as tags")}</label>
            <p class="modal-desc" id="tr-msg" style="min-height:1em"></p>
            <div class="modal-actions">
                <button id="tr-back">${t("Back")}</button>
                <button class="btn-primary" id="tr-preview">${t("Preview")}</button>
            </div>`;
        box.querySelector("#tr-back").onclick = () => stepSource();
        const msg = box.querySelector("#tr-msg");
        box.querySelector("#tr-preview").onclick = async () => {
            let dir = box.querySelector("#tr-dir").value.trim();
            const zip = box.querySelector("#tr-zip").files[0];
            msg.style.color = ""; msg.textContent = zip ? t("Uploading…") : t("Reading…");
            try {
                if (zip) {
                    const fd = new FormData();
                    fd.append("file", zip);
                    const up = await fetch("/api/import/transmission/upload", {
                        method: "POST", headers: { "X-Api-Key": API_KEY }, body: fd,
                    });
                    const ud = await up.json().catch(() => ({}));
                    if (!up.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = ud.error || t("Upload failed"); return; }
                    dir = ud.dir;
                } else if (!dir) {
                    msg.style.color = "var(--accent-red)";
                    msg.textContent = t("Give a folder, or upload a zip of it.");
                    return;
                }
                const req = {
                    dir,
                    categories_from_dirs: box.querySelector("#tr-cats").checked,
                    import_labels: box.querySelector("#tr-labels").checked,
                };
                const res = await fetch("/api/import/transmission/preview", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify(req),
                });
                const d = await res.json().catch(() => ({}));
                if (!res.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = d.error || t("Preview failed ({status})", { status: res.status }); return; }
                stepTransmissionReview(req, d);
            } catch (e) { msg.style.color = "var(--accent-red)"; msg.textContent = t("Error: {msg}", { msg: e.message }); }
        };
    }

    function stepTransmissionReview(req, d) {
        const prefixes = d.path_prefixes || [];
        const rows = prefixes.map(p => `<div style="display:flex;gap:6px;align-items:center;margin-bottom:4px">
            <code style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(p)}">${esc(p)}</code>
            <span>→</span>
            <input type="text" class="tr-map" data-from="${esc(p)}" value="${esc(p)}" style="flex:1">
        </div>`).join("");
        const probs = (d.problems || []).length;
        box.innerHTML = `<h3>${t("Import preview")}</h3>
            <p class="modal-desc">${t("<b>{total}</b> torrents · <b>{complete}</b> complete (seed-mode) · <b>{partial}</b> partial · <b>{stopped}</b> stopped in Transmission · carried upload <b>{carried}</b>", { total: d.total, complete: d.completed, partial: d.incomplete, stopped: d.stopped, carried: formatBytes(d.carried_uploaded_bytes) })}</p>
            <p class="modal-desc">${t("Categories to create: {list}, all as <b>hoard</b>.", { list: (d.categories || []).map(c => esc(c.name)).join(", ") || "-" })}</p>
            ${probs ? `<p class="modal-desc" style="color:var(--accent-red)">${t("{n} file(s) unreadable, they will be skipped.", { n: probs })}</p>` : ""}
            ${d.without_resume ? `<p class="modal-desc">${t("{n} torrent(s) have no resume file: no save path, they will be skipped.", { n: d.without_resume })}</p>` : ""}
            <p class="modal-desc" style="margin-bottom:4px">${t("Path mapping (Transmission path → what Hydra sees):")}</p>
            <div style="max-height:150px;overflow:auto;margin-bottom:8px;border:1px solid var(--border);border-radius:4px;padding:6px">${rows || `<i>${t("no paths detected")}</i>`}</div>
            <label style="display:block;margin-bottom:6px"><input type="checkbox" id="tr-stopped" checked> ${t("Import everything stopped, no announce until you start them")}</label>
            <p class="modal-desc" id="tr-msg2" style="min-height:1em"></p>
            <div class="modal-actions">
                <button id="tr-back2">Back</button>
                <button class="btn-primary" id="tr-go">Import ${d.total} torrents</button>
            </div>`;
        box.querySelector("#tr-back2").onclick = () => stepTransmission({ dir: req.dir });
        box.querySelector("#tr-go").onclick = async () => {
            const path_map = {};
            box.querySelectorAll(".tr-map").forEach(inp => {
                const f = inp.dataset.from, t = inp.value.trim();
                if (t && t !== f) path_map[f] = t;
            });
            const msg = box.querySelector("#tr-msg2");
            msg.style.color = ""; msg.textContent = t("Starting…");
            try {
                const res = await fetch("/api/import/transmission/start", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify(Object.assign({}, req, {
                        path_map,
                        start_stopped: box.querySelector("#tr-stopped").checked,
                    })),
                });
                const d2 = await res.json().catch(() => ({}));
                if (!res.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = d2.error || t("Start failed ({status})", { status: res.status }); return; }
                stepProgress();
            } catch (e) { msg.style.color = "var(--accent-red)"; msg.textContent = t("Network error: {msg}", { msg: e.message }); }
        };
    }

    function stepCreds(prefill) {
        box.innerHTML = `<h3>${t("Import from qBittorrent")}</h3>
            <p class="modal-desc">${t("Point Hydra at your qBittorrent WebUI. Hydra seeds the data already on disk (completed torrents skip the hash-check), so nothing is re-downloaded.")}</p>
            <input type="text" id="qb-url" placeholder="http://qbittorrent:8080" style="width:100%;margin-bottom:8px" value="${esc(prefill && prefill.url || "")}">
            <input type="text" id="qb-user" placeholder="${t("Username")}" autocomplete="off" style="width:100%;margin-bottom:8px" value="${esc(prefill && prefill.user || "admin")}">
            <input type="password" id="qb-pass" placeholder="${t("Password")}" autocomplete="off" style="width:100%">
            <p class="modal-desc" id="qb-msg" style="min-height:1em"></p>
            <div class="modal-actions">
                <button id="qb-skip" class="btn-small">${t("Back")}</button>
                <button class="btn-primary" id="qb-preview">${t("Preview")}</button>
            </div>`;
        box.querySelector("#qb-skip").onclick = () => stepSource();
        const msg = box.querySelector("#qb-msg");
        box.querySelector("#qb-preview").onclick = async () => {
            const creds = {
                url: box.querySelector("#qb-url").value.trim(),
                username: box.querySelector("#qb-user").value,
                password: box.querySelector("#qb-pass").value,
            };
            if (!creds.url) { msg.textContent = t("Enter the qBittorrent URL."); return; }
            msg.style.color = ""; msg.textContent = t("Connecting…");
            try {
                const res = await fetch("/api/import/qbit/preview", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify(creds),
                });
                const d = await res.json().catch(() => ({}));
                if (!res.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = d.error || t("Preview failed ({status})", { status: res.status }); return; }
                stepReview(creds, d);
            } catch (e) { msg.style.color = "var(--accent-red)"; msg.textContent = t("Network error: {msg}", { msg: e.message }); }
        };
        box.querySelector("#qb-pass").addEventListener("keydown", e => { if (e.key === "Enter") box.querySelector("#qb-preview").click(); });
    }

    function stepReview(creds, d) {
        const prefixes = d.path_prefixes || [];
        const rows = prefixes.map(p => `<div style="display:flex;gap:6px;align-items:center;margin-bottom:4px">
            <code style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(p)}">${esc(p)}</code>
            <span>→</span>
            <input type="text" class="qb-map" data-from="${esc(p)}" value="${esc(p)}" style="flex:1">
            <span class="qb-map-st" style="flex:0 0 1.4em;text-align:center;font-weight:700"></span>
        </div>`).join("");
        box.innerHTML = `<h3>${t("Import preview")}</h3>
            <p class="modal-desc">${t("<b>{total}</b> torrents · <b>{complete}</b> complete (seed-mode) · <b>{partial}</b> partial (verify + resume) · carried upload <b>{carried}</b>", { total: d.total, complete: d.completed, partial: d.incomplete, carried: formatBytes(d.carried_uploaded_bytes) })}</p>
            <p class="modal-desc">${t("Categories: {list}, all imported as <b>hoard</b>.", { list: (d.categories || []).map(c => esc(c.name)).join(", ") || "-" })}</p>
            <p class="modal-desc" style="margin-bottom:4px">${t("Path mapping (qBit path → what Hydra sees). Fix these if Hydra mounts the data elsewhere:")}</p>
            <div style="max-height:160px;overflow:auto;margin-bottom:8px;border:1px solid var(--border);border-radius:4px;padding:6px">${rows || `<i>${t("no paths detected")}</i>`}</div>
            <div style="display:flex;gap:8px;align-items:center;margin-bottom:8px">
                <p class="modal-desc" id="qb-data" style="margin:0;flex:1${d.data_checked && d.data_found === 0 ? ";color:var(--accent-red);font-weight:600" : ""}">${!d.data_checked ? "" : (d.data_found === 0
                    ? t("No data found for any of the {checked} torrents sampled. Nothing is reachable at these paths, so every torrent would restart from zero. Fix the mapping above.", { checked: d.data_checked })
                    : t("Data found for {found} of {checked} torrents sampled.", { found: d.data_found, checked: d.data_checked }))}</p>
                <button id="qb-check" style="flex:0 0 auto;width:auto">${t("Re-check paths")}</button>
            </div>
            <label style="display:block;margin-bottom:6px"><input type="checkbox" id="qb-stopped" checked> ${t("Import everything stopped, no announce until you start them")}</label>
            <p class="modal-desc" id="qb-msg2" style="min-height:1em"></p>
            <div class="modal-actions">
                <button id="qb-back">${t("Back")}</button>
                <button class="btn-primary" id="qb-go">${t("Import {n} torrents", { n: d.total })}</button>
            </div>`;
        box.querySelector("#qb-check").onclick = async () => {
            // Checks the mapped folders only, not every payload: this runs while
            // the user is still editing, and it must stay instant on a library
            // the preview took a while to list.
            const inputs = Array.from(box.querySelectorAll(".qb-map"));
            const info = box.querySelector("#qb-data");
            info.style.color = ""; info.style.fontWeight = "";
            info.textContent = t("Checking…");
            try {
                const res = await fetch("/api/import/check-paths", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify({ paths: inputs.map(i => i.value.trim()) }),
                });
                const r = await res.json().catch(() => ({}));
                const results = r.results || [];
                let good = 0;
                inputs.forEach((inp, i) => {
                    const st = inp.parentElement.querySelector(".qb-map-st");
                    const found = !!(results[i] && results[i].exists);
                    if (found) good++;
                    st.textContent = found ? "\u2713" : "\u2717";
                    st.style.color = found ? "var(--accent-green)" : "var(--accent-red)";
                });
                const bad = inputs.length - good;
                if (bad === 0) {
                    info.style.color = "var(--accent-green)";
                    info.textContent = t("All {n} mapped folders are reachable by Hydra.", { n: good });
                } else {
                    info.style.color = "var(--accent-red)"; info.style.fontWeight = "600";
                    info.textContent = t("{bad} of {n} mapped folders are not reachable by Hydra. Those torrents would restart from zero.", { bad: bad, n: inputs.length });
                }
            } catch (e) {
                info.style.color = "var(--accent-red)";
                info.textContent = t("Network error: {msg}", { msg: e.message });
            }
        };
        box.querySelector("#qb-back").onclick = () => stepCreds({ url: creds.url, user: creds.username });
        box.querySelector("#qb-go").onclick = async () => {
            const path_map = {};
            box.querySelectorAll(".qb-map").forEach(inp => {
                const f = inp.dataset.from, t = inp.value.trim();
                if (t && t !== f) path_map[f] = t;
            });
            const msg = box.querySelector("#qb-msg2");
            msg.style.color = ""; msg.textContent = t("Starting…");
            try {
                const res = await fetch("/api/import/qbit/start", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify(Object.assign({}, creds, {
                        path_map,
                        start_stopped: box.querySelector("#qb-stopped").checked,
                    })),
                });
                const d2 = await res.json().catch(() => ({}));
                if (!res.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = d2.error || t("Start failed ({status})", { status: res.status }); return; }
                stepProgress();
            } catch (e) { msg.style.color = "var(--accent-red)"; msg.textContent = t("Network error: {msg}", { msg: e.message }); }
        };
    }

    function stepProgress() {
        box.innerHTML = `<h3 id="qb-title">${t("Importing…")}</h3>
            <p class="modal-desc" id="qb-phase">${t("Connecting…")}</p>
            <div style="background:var(--border);border-radius:4px;height:10px;overflow:hidden;margin:8px 0">
                <div id="qb-bar" style="height:100%;width:0;background:var(--accent-hoard);transition:width .2s"></div>
            </div>
            <p class="modal-desc" id="qb-stats"></p>
            <p class="modal-desc" id="qb-cur" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;opacity:.7"></p>
            <div class="modal-actions"><button class="btn-primary" id="qb-done" style="display:none">${t("Close &amp; reload")}</button></div>`;
        const q = API_KEY ? ("?apikey=" + encodeURIComponent(API_KEY)) : "";
        const es = new EventSource("/api/import/qbit/events" + q);
        const title = box.querySelector("#qb-title"),
            phase = box.querySelector("#qb-phase"), bar = box.querySelector("#qb-bar"),
            stats = box.querySelector("#qb-stats"), cur = box.querySelector("#qb-cur"),
            doneBtn = box.querySelector("#qb-done");
        const labels = { connect: t("Connecting…"), categories: t("Creating categories…"), torrents: t("Importing torrents…"), done: t("Done"), error: t("Error") };
        es.onmessage = (ev) => {
            let d; try { d = JSON.parse(ev.data); } catch (e) { return; }
            phase.style.color = ""; phase.textContent = labels[d.phase] || d.phase;
            if (d.total > 0) bar.style.width = Math.round(100 * d.done / d.total) + "%";
            stats.textContent = t("{done}/{total} · {seeding} seeding · {resuming} resuming · {failed} failed", {
                done: d.done, total: d.total, seeding: d.seeded, resuming: d.downloading, failed: d.failed })
                + (d.skipped ? " · " + t("{n} skipped", { n: d.skipped }) : "");
            cur.textContent = d.current ? t("last: {name}", { name: d.current }) : "";
            if (d.phase === "error") { phase.style.color = "var(--accent-red)"; phase.textContent = t("Error: {msg}", { msg: d.error || t("unknown") }); }
            if (d.finished) {
                es.close();
                // The heading has to stop saying "Importing…" too, or it sits
                // there contradicting the line right under it.
                if (d.phase === "error") {
                    title.textContent = t("Import failed");
                } else {
                    bar.style.width = "100%";
                    title.textContent = t("Import complete");
                    phase.textContent = labels.done; // not the title again
                }
                doneBtn.style.display = "";
                doneBtn.onclick = () => location.reload();
            }
        };
        es.onerror = () => { /* EventSource auto-retries; a finished job's stream 404s and stops */ };
    }

    stepSource();
}
window.importFromQbit = () => importWizard({ fromSettings: true });

// Offer the import once on a fresh instance (no recorded provenance yet).
function maybeOfferImport() {
    if (!API_KEY) return;
    fetch("/api/provenance", { headers: { "X-Api-Key": API_KEY } })
        .then(r => r.json())
        .then(p => { _provenance = p; if (p && !p.present && !localStorage.getItem("hydra_import_dismissed")) importWizard(); })
        .catch(() => { });
}

async function api(endpoint, options = {}) {
    const headers = { "X-Api-Key": API_KEY, ...options.headers };
    const res = await fetch(endpoint, { ...options, headers });
    if (res.status === 401) {
        promptLogin(t("Session invalid, please sign in again."));
        throw new Error("API error: 401");
    }
    if (!res.ok) throw new Error(`API error: ${res.status}`);
    return res.json();
}

// ─── Tabs ───────────────────────────────────────────────

// Edits typed but never saved are lost the moment the page is swapped out, with
// nothing to show they existed. Everything below exists to ask first.
let _netOrig = null;

function _settingsDirtyCount() {
    return _pendingEdits().count;
}

// Counts the edits and says whether applying them needs a restart, so the
// dialog can offer the action that actually finishes the job.
function _pendingEdits() {
    let count = 0, needsRestart = false;
    for (const [id, orig] of Object.entries(_settingsOrig || {})) {
        try {
            if (_readSettingField(id, orig) === orig.value) continue;
            count++;
            if (_settingTier(orig.section, orig.key) !== "hot") needsRestart = true;
        } catch (e) {}
    }
    if (_netState && _netOrig) {
        const cur = netModeCollect();
        if (cur && JSON.stringify({ mode: _netState.mode, fields: cur }) !== _netOrig) {
            count++;
            // The network mode rewrites the engines' listen and proxy keys,
            // which only take effect on restart.
            needsRestart = true;
        }
    }
    return { count: count, needsRestart: needsRestart };
}

// Three ways out, all explicit: save and go, drop them and go, or stay. No
// default action, since two of the three lose work.
function _confirmLeaveSettings(count, onLeave) {
    const pending = _pendingEdits();
    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    ov.innerHTML = `<div class="modal-box">
        <h3>${t("Unsaved settings")}</h3>
        <p class="modal-desc">${esc(t("{n} setting(s) changed and not saved. Leaving this page discards them.", { n: count }))}</p>
        <div class="modal-actions">
            <button class="btn-small" id="leave-stay">${t("Stay here")}</button>
            <button class="btn-small btn-danger" id="leave-discard">${t("Discard and leave")}</button>
            <button class="btn-primary" id="leave-save">${pending.needsRestart ? t("Save and restart") : t("Save and leave")}</button>
        </div>
    </div>`;
    document.body.appendChild(ov);
    const close = () => ov.remove();
    ov.querySelector("#leave-stay").onclick = close;
    ov.querySelector("#leave-discard").onclick = () => { close(); onLeave(); };
    ov.querySelector("#leave-save").onclick = async () => {
        close();
        try { await saveSettings(); } catch (e) { return; }
        if (pending.needsRestart) {
            // Restarting reloads the page, so there is nothing to leave to: the
            // alternative was dropping the user elsewhere with a restart still
            // owed and the notice about it on the page they just left.
            await restartDaemon(true);
            return;
        }
        onLeave();
    };
}

function activateTab(name) {
    // Only when actually leaving the settings page, and only with real edits.
    const current = document.querySelector(".tab.active");
    if (current && current.dataset.tab === "config" && name !== "config") {
        const dirty = _settingsDirtyCount();
        if (dirty > 0) {
            _confirmLeaveSettings(dirty, () => _activateTabNow(name));
            return;
        }
    }
    _activateTabNow(name);
}

function _activateTabNow(name) {
    const tab = document.querySelector(`.tab[data-tab="${name}"]`);
    const content = document.getElementById("tab-" + name);
    if (!tab || !content) return;
    document.querySelectorAll(".tab").forEach(t => t.classList.remove("active"));
    document.querySelectorAll(".tab-content").forEach(c => c.classList.remove("active"));
    tab.classList.add("active");
    content.classList.add("active");
    window.location.hash = name;
    if (name !== "logs") stopLogsTail();
    if (name === "config") updateSettings();
    else if (name === "add") refreshCategoryOptions();
    else if (name === "changelog") loadChangelog();
    else if (name === "agents") { updateAgents(); }
    else if (name === "trackers") { updateTrackers(); loadTrackerStats(); }
    else if (name === "logs") loadLogs();
    else if (name === "jobs") startJobsPolling();
    if (name !== "jobs") stopJobsPolling();
}

document.querySelectorAll(".tab").forEach(tab => {
    tab.addEventListener("click", () => activateTab(tab.dataset.tab));
});

// Restore tab from URL hash on load. Deferred to DOMContentLoaded so module-level
// state declared later in this file (e.g. _logsInit) is already initialized when a
// tab loader runs. A parse-time activateTab("logs") otherwise hits a TDZ
// ReferenceError ("Cannot access '_logsInit' before initialization"), the fetch
// never fires, and the Logs tab stays stuck on "Loading..." after a refresh.
window.addEventListener("DOMContentLoaded", () => {
    const _hashTab = window.location.hash.replace("#", "");
    if (!_hashTab || !document.getElementById("tab-" + _hashTab)) return;
    activateTab(_hashTab);
    window.addEventListener("load", async () => {
        if (_hashTab === "race") await updateRaceTorrents();
        else if (_hashTab === "hoard") await updateHoardStats();
        else if (_hashTab === "categories") await updateCategories();
        else if (_hashTab === "agents") { await updateAgents(); }
        else if (_hashTab === "trackers") { await updateTrackers(); loadTrackerStats(); }
        else if (_hashTab === "benchmark") await updateBenchmark();
        else if (_hashTab === "jobs") startJobsPolling();
    });
});

// Bandeau "data_dir sur stockage reseau". Le warning existe aussi au demarrage
// dans les logs, mais un log defile et personne ne le relit : la consequence
// (base corrompue par une coupure du partage) merite d etre visible la ou l
// utilisateur regarde. Masquable, et la version masquee est retenue par kind :
// changer de type de montage le reaffiche.
// Set once the daemon says it is waiting to be repaired. The startup poller
// reads it: there is no state to restore and never will be until the database
// is converted, so the boot overlay must stop pretending otherwise.
let STORE_REPAIR_MODE = false;

// Blocking screen for a daemon that cannot open its database.
//
// This is not a warning the user can dismiss: until the database is converted
// there is nothing running behind the interface. The one action offered copies
// each file aside before touching it, because the alternative to a recoverable
// problem should never be an unrecoverable one.
async function showStoreRepairModal() {
    let st = {};
    try { st = (await (await fetch("/api/store/repair")).json()) || {}; } catch (_) {}
    const targets = st.targets || [];
    const hot = targets.filter(x => x.hot_wal);

    STORE_REPAIR_MODE = true;
    // The boot overlay polls for a startup that is never coming, and covers the
    // whole viewport while it does. Take it out before saying anything.
    const boot = document.getElementById("startup-overlay");
    if (boot) boot.remove();

    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    ov.innerHTML = `<div class="modal-box">
        <h3>${t("Hydra cannot open its database")}</h3>
        <p class="modal-desc">${t("Your data_dir now points at {kind} storage. The database was created on a local disk, so it uses a write-ahead log, and a network share cannot host one.", { kind: esc(st.filesystem || "network") })}</p>
        <p class="modal-desc">${t("Nothing has been lost. Hydra has deliberately not started its engines: carrying on without the database is what would destroy your lifetime upload counters.")}</p>
        <p class="modal-desc">${t("Affected: {list}", { list: esc(targets.map(x => x.name).join(", ") || "-") })}</p>
        ${hot.length ? `<p class="modal-desc" style="color:var(--accent-orange)">${t("{list} still holds changes that were never written back. Start Hydra once on the machine it came from, stop it cleanly, then move it here.", { list: esc(hot.map(x => x.name).join(", ")) })}</p>` : ""}
        <p class="modal-desc">${t("Hydra can convert the database to the journal a share can host. Every file is copied to a .bak alongside it first, so the originals stay recoverable whatever happens.")}</p>
        <p class="modal-desc" id="repair-msg" style="min-height:1em"></p>
        <div class="modal-actions">
            <button class="btn-primary" id="repair-go">${t("Back up and convert")}</button>
        </div>
    </div>`;
    document.body.appendChild(ov);

    const msg = ov.querySelector("#repair-msg");
    let btn = ov.querySelector("#repair-go");

    // Second half of the flow: the databases are converted, but this process
    // registered its routes at boot and has no engines behind them. Only a
    // restart finishes the job, so that is the only thing left to offer.
    const offerRestart = (results) => {
        msg.style.color = "var(--accent-green, #3a9)";
        msg.innerHTML = (results || []).map(r =>
            t("{name}: converted. Backup kept at {backup}", { name: esc(r.name), backup: esc(r.backup || "-") })
        ).join("<br>") + "<br>" + t("Restart Hydra to finish.");
        const fresh = btn.cloneNode(true); // drops the convert handler
        fresh.textContent = t("Restart Hydra");
        fresh.disabled = false;
        btn.replaceWith(fresh);
        btn = fresh;
        btn.addEventListener("click", async () => {
            btn.disabled = true;
            try { await fetch("/api/settings/restart", { method: "POST", headers: { "X-Api-Key": API_KEY } }); } catch (_) {}
            msg.textContent = t("Hydra is stopping. Under Docker or systemd it comes back on its own; if you started it by hand, start it again.");
            setTimeout(() => location.reload(), 8000);
        });
    };

    // Reloading after a successful conversion lands here. Offering to convert
    // again would be wrong twice over: the work is done, and the reason the
    // interface still will not load is the pending restart, not the database.
    if (st.ran && st.repaired) {
        offerRestart(st.results);
        return;
    }

    btn.addEventListener("click", async () => {
        btn.disabled = true;
        msg.style.color = "";
        msg.textContent = t("Backing up, then converting. Do not stop Hydra.");
        let res, body = {};
        try {
            res = await fetch("/api/store/repair", { method: "POST", headers: { "X-Api-Key": API_KEY } });
            body = (await res.json()) || {};
        } catch (e) {
            msg.style.color = "var(--accent-red)";
            msg.textContent = t("The daemon did not answer: {err}", { err: String(e) });
            btn.disabled = false;
            return;
        }
        if (res.status === 401) {
            // Step aside rather than stacking a second full-screen overlay on
            // the first: both carry the same z-index, so which one takes the
            // clicks is a matter of DOM order, and a login box you cannot type
            // into is worse than no login box. Signing in ends in a reload,
            // which brings this screen straight back, key in hand.
            ov.remove();
            promptLogin(t("Signing in lets Hydra repair the database."));
            return;
        }
        const results = body.results || [];
        const failed = results.filter(r => r.error);
        if (!body.repaired || failed.length) {
            msg.style.color = "var(--accent-red)";
            msg.innerHTML = failed.map(r => t("{name}: {err}", { name: esc(r.name), err: esc(r.error) })).join("<br>")
                || t("The conversion did not complete.");
            btn.disabled = false;
            return;
        }
        offerRestart(results);
    });
}

window.addEventListener("DOMContentLoaded", () => { _refreshCtxAgents(); });

window.addEventListener("DOMContentLoaded", async () => {
    _refreshHoardPins();
    // Pick the language and translate the static markup before anything else
    // paints. Everything in the DOM at this point came from index.html.
    try { await I18N.load(I18N.detect()); I18N.translateDOM(document.body); } catch (e) {}

    let setup = null;
    try {
        setup = (await (await fetch("/api/setup")).json()) || {};
    } catch (_) { return; }
    // The daemon came up unable to open its database. Nothing else in the UI
    // has anything to show: there are no engines behind it. Explain, offer the
    // repair, and go no further.
    if (setup.store_repair) return showStoreRepairModal();
    const kind = setup.network_storage || "";
    if (!kind || localStorage.getItem("hydra_netfs_ack") === kind) return;
    const bar = document.createElement("div");
    bar.style.cssText = "padding:10px 14px;background:var(--accent-orange,#c87f0a);color:#fff;" +
        "font-size:13px;display:flex;gap:12px;align-items:center;justify-content:center";
    bar.innerHTML = "<span>" + t("Config on network storage detected ({kind}). Hydra can handle this, but the database cannot use its safest journal mode there: a share that drops mid-write can corrupt it. <b>Keep backups of your data_dir</b>, or move data_dir to a local disk (your downloads can stay on the share).", { kind: esc(kind) }) + "</span>";
    const btn = document.createElement("button");
    btn.textContent = t("Got it");
    btn.className = "btn-secondary";
    // The bar is a flex row: without this the button is a shrinkable flex item
    // and the text wraps it onto two lines.
    btn.style.cssText = "flex:0 0 auto;white-space:nowrap";
    btn.addEventListener("click", () => { localStorage.setItem("hydra_netfs_ack", kind); bar.remove(); });
    bar.appendChild(btn);
    document.body.prepend(bar);
});

// ─── Mode selector ──────────────────────────────────────

document.querySelectorAll(".mode-btn").forEach(btn => {
    btn.addEventListener("click", () => {
        document.querySelectorAll(".mode-btn").forEach(b => b.classList.remove("active"));
        btn.classList.add("active");
        const isHoard = btn.dataset.mode === "hoard";
        document.getElementById("magnet-group").style.display = isHoard ? "none" : "block";
    });
});

// ─── Health check ───────────────────────────────────────

async function checkHealth() {
    try {
        const data = await api("/health");
        if (data.version) {
            const v = "v" + data.version;
            document.getElementById("header-version").textContent = v;
            document.getElementById("footer-version").textContent = v;
            checkForUpdate();
        }
    } catch {
        // Do not touch the health dot here: it reflects port-forward / listen
        // health (owned by fetchPortForward). A single transient /health blip on
        // a loaded box must not latch it red until the next 60s port check.
    }
}

// ─── Overview polling ───────────────────────────────────

// _renderStatus applies a /api/status payload to the DOM. Called both by
// the legacy fetch path (updateOverview, used on tab change) and by the SSE
// status_snapshot handler. Pure render, does no I/O.
function _renderStatus(data) {
        // Race stats
        if (data.race) {
            document.getElementById("race-count").textContent = data.race.torrents;
            document.getElementById("race-with-peers").textContent = data.race.torrents_with_peers || 0;
            document.getElementById("race-upload").textContent = formatSpeed(data.race.total_upload_rate);
            document.getElementById("race-download").textContent = formatSpeed(data.race.total_download_rate);
            document.getElementById("race-peers").textContent = data.race.total_peers;
            document.getElementById("race-ov-ratio").textContent = (data.race.session_ratio || 0).toFixed(2);

            // Race tab stats bar
            document.getElementById("race-active-dl").textContent = data.race.active_downloads || 0;
            document.getElementById("race-active-seeds").textContent = data.race.active_seeds || 0;
            document.getElementById("race-current-ul").textContent = formatSpeed(data.race.total_upload_rate);
            document.getElementById("race-session-ratio").textContent = (data.race.session_ratio || 0).toFixed(2);
            document.getElementById("race-session-grabbed").textContent = data.race.session_grabbed || 0;

            // Header speeds (Option 2 : Gbps)
            const _raceUlRateCurrent = data.race.total_upload_rate || 0; window._lastRaceUlRate = _raceUlRateCurrent; window._lastRaceDlRate = data.race.total_download_rate || 0; const hydraUl = _raceUlRateCurrent + (data.hoard?.active_upload_rate || 0);
            const hydraDl = data.race.total_download_rate + (data.hoard?.active_download_rate || 0);
            document.getElementById("total-upload").textContent = formatSpeed(hydraUl);
            document.getElementById("total-download").textContent = formatSpeed(hydraDl);
        }

        // Hoard stats
        if (data.hoard) {
            document.getElementById("hoard-total").textContent = data.hoard.total_torrents;
            document.getElementById("hoard-with-peers").textContent = data.hoard.torrents_with_peers;
            document.getElementById("hoard-connections").textContent = data.hoard.active_peers;
            document.getElementById("hoard-upload").textContent = formatSpeed(data.hoard.active_upload_rate);
            document.getElementById("hoard-download").textContent = formatSpeed(data.hoard.active_download_rate);
            const hSU = data.hoard.session_uploaded || 0, hSD = data.hoard.session_downloaded || 0;
            document.getElementById("hoard-ov-ratio").textContent = (hSD > 0 ? hSU / hSD : 0).toFixed(2);
            const ovt = document.getElementById("ov-torrents-total");
            if (ovt) ovt.textContent = ((data.hoard.total_torrents || 0) + (data.race?.torrents || 0)).toLocaleString();
        }

        // Day totals, UL/DL accumulated since midnight Europe/Paris (auto reset)
        const dayUl = data.day_uploaded || 0;
        const dayDl = data.day_downloaded || 0;
        document.getElementById("day-upload").textContent = formatBytes(dayUl);
        document.getElementById("day-download").textContent = formatBytes(dayDl);

        // All-time totals (baseline + current Rain session)
        if (data.baseline) {
            const globalUl = data.baseline.global_uploaded || 0;
            const globalDl = data.baseline.global_downloaded || 0;
            document.getElementById("total-ul-alltime").textContent = formatBytes(globalUl);
            document.getElementById("total-dl-alltime").textContent = formatBytes(globalDl);

            // Ratio (header + overview cap)
            const rStr = (globalDl > 0 ? globalUl / globalDl : 0).toFixed(2);
            const hr = document.getElementById("header-ratio"); if (hr) hr.textContent = rStr;
            const ovr = document.getElementById("ov-ratio"); if (ovr) ovr.textContent = rStr;
            const ovdl = document.getElementById("ov-dl-total"); if (ovdl) ovdl.textContent = formatBytes(globalDl);

            // Compteur milestone PiB DYNAMIQUE (prochain jalon entier), 1 PiB = 2^50 octets
            const PiB = 1125899906842624;
            const pib = globalUl / PiB;
            const goal = Math.floor(pib) + 1;        // prochain jalon
            const numEl = document.getElementById("ov-milestone-num");
            if (numEl) numEl.innerHTML = pib.toFixed(2) + '<span class="u">PiB</span>';
            const curEl = document.getElementById("ov-milestone-cur");
            if (curEl) curEl.textContent = pib.toFixed(2) + " PiB";
            const goalEl = document.getElementById("ov-milestone-goal") || document.querySelector(".milestone .goal");
            if (goalEl) goalEl.innerHTML = "\u25C6 " + goal.toFixed(2) + " PiB";
            const tagEl = document.getElementById("ov-milestone-tag") || document.querySelector(".card.wide h3 .tag");
            if (tagEl) tagEl.innerHTML = "\u25C6 " + t("{n} PiB milestone", { n: goal });
            const fillEl = document.getElementById("ov-milestone-fill");
            if (fillEl) fillEl.style.width = Math.min(100, (pib - (goal - 1)) * 100).toFixed(1) + "%";
            const togoEl = document.getElementById("ov-milestone-togo");
            if (togoEl) {
                const togoTiB = (goal - pib) * 1024;
                togoEl.textContent = t("{n} TiB to go", { n: togoTiB.toFixed(1) });
            }
        }

        // Tunnel stats
        if (data.tunnels && data.tunnels.length > 0) {
            const el = document.getElementById("tunnel-info");
            if (el) {
                const isLeak = el.classList.contains("tunnel-leak");
                const rows = data.tunnels.map(t => {
                    const ipColor = isLeak ? "#f85149" : "";
                    const barColor = isLeak ? "#f85149" : "";
                    return `<div class="tunnel-row">` +
                        `<span class="tunnel-iface"${isLeak ? ' style="color:#f85149"' : ''}>${t.iface}</span>` +
                        `<span class="tunnel-ip"${ipColor ? ` style="color:${ipColor}"` : ''}>${incoIP(t.ip)}</span>` +
                        `<div class="tunnel-bar-wrap"><div class="tunnel-bar" style="width:${t.tx_pct}%${barColor ? ';background:'+barColor : ''}"></div></div>` +
                        `<span class="tunnel-pct"${isLeak ? ' style="color:#f85149"' : ''}>${t.tx_pct}%</span>` +
                        `</div>`;
                });
                el.innerHTML = rows.join("");
                el.title = data.tunnels.map(t => t.iface + ": UL " + formatBytes(t.tx_rate) + "/s  DL " + formatBytes(t.rx_rate) + "/s").join("\n");
            }
        }


        // Peer intel stats
        if (data.peer_intel) {
            const it = document.getElementById("intel-total"); if (it) it.textContent = data.peer_intel.total_peers?.toLocaleString() || "0";
            const is = document.getElementById("intel-seedboxes"); if (is) is.textContent = data.peer_intel.known_seedboxes || "0";
            const ia = document.getElementById("intel-avg-score"); if (ia) ia.textContent = (data.peer_intel.avg_score || 0).toFixed(3);
        }

        // Uptime, moved here from /metrics fetch (now sourced from status payload).
        if (typeof data.uptime === "number") {
            const upEl = document.getElementById("uptime-value");
            if (upEl) upEl.textContent = formatUptime(data.uptime);
        }

        document.getElementById("last-update").textContent =
            t("Last update: {time}", { time: new Date().toLocaleTimeString() });
}


// ─── Records all-time + jalons PiB (source: bench.db via /api/benchmark/records) ───
let _recordsLastFetch = 0;
async function updateRecords(force) {
    const nowMs = Date.now();
    if (!force && nowMs - _recordsLastFetch < 60000) return;
    _recordsLastFetch = nowMs;
    let d;
    try { d = await api("/api/benchmark/records"); } catch (e) { return; }
    if (!d) return;

    // Records card (located by the h3 "Records" title)
    const recCard = [...document.querySelectorAll(".card")].find(c => {
        const h = c.querySelector("h3");
        return h && h.textContent.trim().startsWith("Records");
    });
    const rc = recCard ? recCard.querySelector(".card-body") : null;
    if (rc && Array.isArray(d.records)) {
        rc.innerHTML = d.records.map(r => {
            const val = r.unit ? (r.value + " " + r.unit) : Math.round(r.value).toLocaleString("en-US");
            return '<div class="metric"><span>' + r.label + ' <small>' + (r.date || "") +
                '</small></span><span class="' + (r.hi ? "hi" : "") + '">' + val + '</span></div>';
        }).join("");
    }

    // Milestones (accel section): auto durations + next-milestone ETA line
    const ac = document.getElementById("ov-accel") || document.querySelector(".accel");
    if (ac && Array.isArray(d.milestones)) {
        const ord = ["1st","2nd","3rd","4th","5th","6th","7th","8th","9th","10th"];
        const label = n => t("{ord} pebibyte", { ord: ord[n - 1] || (n + "th") });
        const rows = [];
        if (_provenance && _provenance.present) {
            const since = _provenance.source_date ? new Date(_provenance.source_date * 1000).toLocaleDateString("en-US", { year: "numeric", month: "short" }) : "";
            rows.push('<div class="lead">' + t("same counter · carried over from {client}", { client: esc(_provenance.source_client || t("a previous client")) }) + (since ? (' · ' + t("since {date}", { date: since })) : '') + '</div>');
        }
        d.milestones.forEach((m, i) => {
            if (!m.observed) {
                rows.push('<div class="ar"><span class="when">' + label(m.pib) +
                    '</span><span class="via">libtorrent (C++) &middot; qBittorrent</span><span class="dur">pre-Hydra</span></div>');
            } else {
                const hot = (i === d.milestones.length - 1) ? " hot" : "";
                const dur = m.since_prev || "-";
                rows.push('<div class="ar' + hot + '"><span class="when">' + label(m.pib) +
                    '</span><span class="via">Typhon (Rust) &middot; Hydra &middot; ' + m.date +
                    '</span><span class="dur">' + dur + '</span></div>');
            }
        });
        if (d.next_eta_date) {
            rows.push('<div class="ar"><span class="when">' + label(d.next_pib) +
                '</span><span class="via">projected &middot; ' + d.rate_tib_day +
                ' TiB/day</span><span class="dur">ETA ' + d.next_eta_date + '</span></div>');
        }
        ac.innerHTML = rows.join("");
    }
}

async function updateOverview() {
    try {
        _renderStatus(await api("/api/status"));
        updateRecords();
    } catch (e) {
        console.error("Failed to update overview:", e);
    }
}

// ─── Race torrents ──────────────────────────────────────

let selectedTorrent = null;
let selectedTorrentAgent = "local";
let detailRefreshTimer = null;
let selectedHoardTorrent = null;
let selectedHoardTorrentAgent = "local";
let hoardDetailTimer = null;

// Race sort
let _raceSortCol = localStorage.getItem("hydra_race_sort_col") || "added_time";
let _raceSortAsc = localStorage.getItem("hydra_race_sort_asc") === "1";

function sortRace(th) {
    const col = th.dataset.col;
    if (_raceSortCol === col) {
        _raceSortAsc = !_raceSortAsc;
    } else {
        _raceSortCol = col;
        _raceSortAsc = true;
    }
    localStorage.setItem("hydra_race_sort_col", _raceSortCol);
    localStorage.setItem("hydra_race_sort_asc", _raceSortAsc ? "1" : "0");
    document.querySelectorAll("#race-table thead th").forEach(h => {
        h.classList.remove("sort-asc", "sort-desc");
    });
    th.classList.add(_raceSortAsc ? "sort-asc" : "sort-desc");
    updateRaceTorrents();
}

// Cache hoard torrents
let _hoardAllTorrents = [];
let _hoardLastFetch = 0;
let _hoardStatsPainted = false;
let _hoardStateFilter = "";
let _hoardCatFilter = "";
let _hoardTrackerFilter = "";
let _hoardTagFilter = "";
let _hoardSortCol = localStorage.getItem("hydra_hoard_sort_col") || "added_time";
let _hoardSortAsc = localStorage.getItem("hydra_hoard_sort_asc") === "1";
const HOARD_FETCH_INTERVAL = 30000; // bumped 2026-04-19: SSE /api/events fournit le live, ce poll reste un backstop statique (name, category, scrape)
const HOARD_RENDER_LIMIT = 500;

async function updateRaceTorrents() {
    loadRacePolicy();
    try {
        let torrents = await api("/api/race/torrents");
        const tbody = document.getElementById("race-tbody");

        // Sort indicator
        document.querySelectorAll("#race-table thead th").forEach(h => {
            h.classList.remove("sort-asc", "sort-desc");
            if (h.dataset.col === _raceSortCol) h.classList.add(_raceSortAsc ? "sort-asc" : "sort-desc");
        });

        if (torrents.length === 0) {
            tbody.innerHTML = `<tr><td colspan="${_visibleCols("race-table").length}" class="empty">No race torrents</td></tr>`;
            return;
        }

        // Sort
        const col = _raceSortCol;
        const asc = _raceSortAsc;
        torrents = [...torrents].sort((a, b) => {
            let va = a[col] ?? 0, vb = b[col] ?? 0;
            if (typeof va === "string") {
                const cmp = va.localeCompare(vb);
                if (cmp !== 0) return asc ? cmp : -cmp;
            } else {
                const diff = asc ? va - vb : vb - va;
                if (diff !== 0) return diff;
            }
            return (a.info_hash || "").localeCompare(b.info_hash || "");
        });

        tbody.innerHTML = torrents.map(t => {
            const detailSel = selectedTorrent === t.info_hash ? ' selected' : '';
            return `<tr class="t-row clickable${detailSel}" data-hash="${t.info_hash}" data-mode="race" data-agent="${t.agent || 'local'}" onclick="handleRowClick(event,'${t.info_hash}','race')" oncontextmenu="handleRowContextMenu(event,'${t.info_hash}','race')">${renderRowCells("race-table", t)}</tr>`;
        }).join("");
        _updateRowHighlights();

        // Avg Share = ratio / (swarm_seeds - 1) for completed torrents with seeds > 1
        const completed = torrents.filter(t => t.progress >= 1.0 && t.swarm_seeds > 1 && t.ratio > 0);
        if (completed.length > 0) {
            const avgShare = completed.reduce((sum, t) => sum + t.ratio / (t.swarm_seeds - 1), 0) / completed.length;
            document.getElementById("race-avg-share").textContent = (avgShare * 100).toFixed(1) + "%";
        } else {
            document.getElementById("race-avg-share").textContent = "-";
        }
    } catch (e) {
        console.error("Failed to update race torrents:", e);
    }
}

async function showDetail(infoHash, agent) {
    selectedTorrent = infoHash;
    selectedTorrentAgent = agent || "local";
    const panel = document.getElementById("torrent-detail");
    panel.style.display = "block";

    // Reset to info tab
    switchDetailTab("info");

    // Auto-refresh detail
    if (detailRefreshTimer) clearInterval(detailRefreshTimer);
    await refreshDetail();
    detailRefreshTimer = setInterval(refreshDetail, 3000);
}

function closeDetail() {
    selectedTorrent = null;
    selectedTorrentAgent = "local";
    if (detailRefreshTimer) clearInterval(detailRefreshTimer);
    if (_tlProgressChart) { _tlProgressChart.destroy(); _tlProgressChart = null; }
    if (_tlPeerChart) { _tlPeerChart.destroy(); _tlPeerChart = null; }
    const panel = document.getElementById("torrent-detail");
    if (panel.style.display === "none") return;
    panel.classList.add("closing");
    panel.addEventListener("animationend", () => {
        panel.style.display = "none";
        panel.classList.remove("closing");
    }, { once: true });
}

let _activeDetailTab = "info";
let _detailAddedTime = 0;

function switchDetailTab(tab) {
    _activeDetailTab = tab;
    document.querySelectorAll("#torrent-detail .detail-tab").forEach(btn =>
        btn.classList.toggle("active", btn.textContent.toLowerCase() === tab)
    );
    document.getElementById("detail-tab-info").style.display = tab === "info" ? "" : "none";
    document.getElementById("detail-tab-timeline").style.display = tab === "timeline" ? "" : "none";
    document.getElementById("detail-tab-content").style.display = tab === "content" ? "" : "none";
    if (tab === "timeline" && selectedTorrent) loadTimeline(selectedTorrent);
    if (tab === "content" && selectedTorrent) {
        loadTorrentContent(selectedTorrent, "detail-content-body", "detail-content-summary", selectedTorrentAgent, "race");
    }
}

let _activeHoardDetailTab = "info";

function switchHoardDetailTab(tab) {
    _activeHoardDetailTab = tab;
    document.querySelectorAll("#hoard-detail-panel .h-detail-tab").forEach(btn =>
        btn.classList.toggle("active", btn.textContent.toLowerCase() === tab)
    );
    document.getElementById("h-detail-tab-info").style.display = tab === "info" ? "" : "none";
    document.getElementById("h-detail-tab-content").style.display = tab === "content" ? "" : "none";
    if (tab === "content" && selectedHoardTorrent) {
        loadTorrentContent(selectedHoardTorrent, "h-detail-content-body", "h-detail-content-summary", selectedHoardTorrentAgent, "hoard");
    }
}

// ---------------------------------------------------------------------------
// Content tab, what is actually inside the torrent
// ---------------------------------------------------------------------------

// Guards against a slow response landing after the user selected another
// torrent: only the newest request is allowed to paint.
let _contentReq = 0;

async function loadTorrentContent(infoHash, bodyId, summaryId, agent, mode) {
    const body = document.getElementById(bodyId);
    const summary = document.getElementById(summaryId);
    if (!body) return;
    const req = ++_contentReq;
    body.innerHTML = `<div class="tc-empty">Loading…</div>`;
    if (summary) summary.textContent = "";
    let files, avail;
    try {
        const params = new URLSearchParams();
        if (agent && !_isLocalAgent(agent)) params.set("agent", agent);
        if (mode && agent && !_isLocalAgent(agent)) params.set("mode", mode);
        const q = params.toString() ? `?${params.toString()}` : "";
        const d = await api(`/api/torrents/${infoHash}/files${q}`);
        files = d.files || [];
        avail = d.availability || null;
    } catch (err) {
        if (req !== _contentReq) return;
        body.innerHTML = `<div class="tc-empty">Could not read the file list, ${esc(err.message || String(err))}</div>`;
        return;
    }
    if (req !== _contentReq) return;
    if (files.length === 0) {
        body.innerHTML = `<div class="tc-empty">The engine reports no files for this torrent.</div>`;
        return;
    }
    const total = files.reduce((a, f) => a + (f.size || 0), 0);
    const sorted = files.slice().sort((a, b) => (b.size || 0) - (a.size || 0));
    const rows = sorted.map(f => {
        const share = total > 0 ? (f.size || 0) * 100 / total : 0;
        return `<tr>
            <td class="tc-path">${esc(f.path || "")}</td>
            <td class="tc-num">${formatBytes(f.size || 0)}</td>
            <td class="tc-num">${share.toFixed(1)}%</td>
            <td class="tc-barcell"><div class="tc-bar"><div class="tc-bar-fill" style="width:${share.toFixed(1)}%"></div></div></td>
        </tr>`;
    }).join("");
    if (summary) {
        let line = tp(files.length, "{n} file", "{n} files") + " · " + formatBytes(total);
        // Seeding torrents carry no piece map, so there is no availability to
        // show. Say that instead of printing a misleading zero.
        if (avail) {
            line += " · " + t("availability {range} (avg {avg} over {pieces} pieces)", {
                range: avail.min + (avail.max !== avail.min ? "-" + avail.max : ""),
                avg: avail.avg.toFixed(2), pieces: avail.num_pieces });
        } else {
            line += " · " + t("availability n/a (seeding, no piece map)");
        }
        summary.textContent = line;
    }
    body.innerHTML = `<div class="tc-scroll"><table class="tc-table">
        <thead><tr><th>${t("Path")}</th><th class="tc-num">${t("Size")}</th><th class="tc-num">${t("Share")}</th><th></th></tr></thead>
        <tbody>${rows}</tbody>
    </table></div>`;
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

let _tlProgressChart = null;
let _tlPeerChart = null;
const _tlPeerColors = ["#f0883e","#f85149","#d2a8ff","#58a6ff","#3fb950","#db61a2","#79c0ff","#ffa657","#7ee787","#ff7b72"];

async function loadTimeline(infoHash) {
    try {
        const data = await api(`/api/race/timeline/${infoHash}`);
        const events = data.events || [];
        let snapshots = data.snapshots || [];

        if (events.length === 0 && snapshots.length === 0) {
            document.getElementById("tl-result").style.display = "none";
            document.getElementById("tl-meta").style.display = "none";
            document.getElementById("tl-events-log").innerHTML = '<div class="empty">No timeline data</div>';
            return;
        }

        // Truncate at completion: past 100% the snapshots are useless for analyzing the race.
        const firstCompleteIdx = snapshots.findIndex(s => (s.progress || 0) >= 1.0);
        if (firstCompleteIdx !== -1) {
            snapshots = snapshots.slice(0, firstCompleteIdx + 1);
        }

        const addedEv = events.find(e => e.event === "added");
        const completedEv = events.find(e => e.event === "completed");
        const t0 = addedEv ? addedEv.ts : (snapshots.length > 0 ? snapshots[0].ts : 0);

        // Each render is isolated: a failure in one (e.g. Chart.js unavailable)
        // must not swallow the rest, notably the peers list and event log.
        const safe = (fn, label) => { try { fn(); } catch (e) { console.error("timeline render failed:", label, e); } };

        safe(() => renderTimelineResult(events, snapshots, t0), "result");
        safe(() => renderTimelineMeta(events, snapshots, t0), "meta");
        if (typeof Chart !== "undefined") {
            safe(() => renderProgressChart(snapshots, events, t0), "progress-chart");
            safe(() => renderPeerSpeedChart(snapshots, t0), "peer-chart");
        } else {
            console.warn("Chart.js unavailable, skipping timeline graphs");
        }
        safe(() => renderTimelineEvents(events, t0), "events");

        // Show first snapshot peers
        if (snapshots.length > 0) {
            const mid = snapshots[Math.floor(snapshots.length / 2)];
            safe(() => renderTimelinePeers(mid, t0), "peers");
        }
    } catch (e) {
        console.error("Failed to load timeline:", e);
    }
}

function renderTimelineResult(events, snapshots, t0) {
    const el = document.getElementById("tl-result");
    const completedEv = events.find(e => e.event === "completed");
    if (!completedEv) { el.style.display = "none"; return; }

    const dlTime = completedEv.download_time || (completedEv.ts - t0);

    // Find competitors, exclude initial seeders (already at 100% in first snapshot)
    const initialSeeders = new Set();
    const seenAsLeecher = new Set();  // peers we saw with progress < 1.0
    let firstSnapParsed = false;
    for (const snap of snapshots) {
        if (!snap.peers_json || snap.peers_json === "[]") continue;
        try {
            const peers = JSON.parse(snap.peers_json);
            if (!firstSnapParsed) {
                // First snapshot with peers, mark initial seeders
                for (const p of peers) {
                    if (p.progress >= 1.0) initialSeeders.add(p.ip);
                }
                firstSnapParsed = true;
            }
            for (const p of peers) {
                if (p.progress > 0 && p.progress < 1.0) seenAsLeecher.add(p.ip);
            }
        } catch(_) {}
    }
    // Track when competitors (non-initial-seeders) finished
    let winnerTime = dlTime;
    const competitorFinish = {};
    for (const snap of snapshots) {
        if (!snap.peers_json || snap.peers_json === "[]") continue;
        try {
            const peers = JSON.parse(snap.peers_json);
            for (const p of peers) {
                if (p.progress >= 1.0 && !competitorFinish[p.ip] && !initialSeeders.has(p.ip)) {
                    // Only count peers we actually saw leeching, or appeared after race start
                    if (seenAsLeecher.has(p.ip)) {
                        competitorFinish[p.ip] = snap.ts - t0;
                    }
                }
            }
        } catch(_) {}
    }
    // Estimate position
    let position = 1;
    for (const ip in competitorFinish) {
        if (competitorFinish[ip] < dlTime) position++;
        if (competitorFinish[ip] < winnerTime) winnerTime = competitorFinish[ip];
    }
    const total = seenAsLeecher.size + 1;

    const won = position === 1;
    el.className = `tl-result ${won ? "win" : "loss"}`;
    el.style.display = "flex";
    el.innerHTML = `
        <div>
            <div class="tl-pos" style="color:${won ? '#3fb950' : '#f85149'}">${position===1?t("{n}st",{n:position}):position===2?t("{n}nd",{n:position}):position===3?t("{n}rd",{n:position}):t("{n}th",{n:position})} <span>/ ${total}</span></div>
        </div>
        <div class="tl-detail">
            ${!won ? `<div style="color:#f85149;font-weight:600">${t("Lost by {delta}", { delta: formatDuration(dlTime - winnerTime) })}</div>` : `<div style="color:#3fb950;font-weight:600">${t("Winner!")}</div>`}
            ${!won && winnerTime > 0 ? `<div style="color:var(--text-muted)">${t("Winner {factor}× faster", { factor: (dlTime/winnerTime).toFixed(1) })}</div>` : ''}
        </div>`;
}

function renderTimelineMeta(events, snapshots, t0) {
    const el = document.getElementById("tl-meta");
    const completedEv = events.find(e => e.event === "completed");
    if (!completedEv || snapshots.length === 0) { el.style.display = "none"; return; }

    const dlTime = completedEv.download_time || (completedEv.ts - t0);
    const size = completedEv.size || 0;
    const avgDL = dlTime > 0 && size > 0 ? size / dlTime : 0;
    const peakDL = Math.max(...snapshots.map(s => s.download_rate || 0));
    const finalRatio = completedEv.upload_total > 0 && completedEv.download_total > 0
        ? (completedEv.upload_total / completedEv.download_total) : 0;

    el.style.display = "grid";
    el.innerHTML = `
        <div class="tl-meta-item"><div class="tl-meta-label">${t("Size")}</div><div class="tl-meta-val">${formatBytes(size)}</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">${t("Duration")}</div><div class="tl-meta-val">${formatDuration(dlTime)}</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">${t("Avg DL")}</div><div class="tl-meta-val">${formatBytes(avgDL)}/s</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">${t("Peak DL")}</div><div class="tl-meta-val good">${formatBytes(peakDL)}/s</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">${t("Ratio")}</div><div class="tl-meta-val">${finalRatio.toFixed(2)}</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">${t("Peers max")}</div><div class="tl-meta-val">${Math.max(...snapshots.map(s => s.peers || 0))}</div></div>`;
}

function renderProgressChart(snapshots, events, t0) {
    if (_tlProgressChart) { _tlProgressChart.destroy(); _tlProgressChart = null; }
    const canvas = document.getElementById("tl-progress-chart");
    if (!canvas || snapshots.length === 0) return;

    const labels = snapshots.map(s => formatDuration(s.ts - t0));

    // Our progress
    const ourProgress = snapshots.map(s => (s.progress || 0) * 100);
    // Our DL rate
    const ourDLRate = snapshots.map(s => s.download_rate || 0);

    // Competitor progress lines (from peers_json)
    const compData = {};
    for (let i = 0; i < snapshots.length; i++) {
        const snap = snapshots[i];
        if (!snap.peers_json || snap.peers_json === "[]") continue;
        try {
            const peers = JSON.parse(snap.peers_json);
            for (const p of peers) {
                if (p.progress > 0 && p.progress < 1.0) {
                    if (!compData[p.ip]) compData[p.ip] = { data: new Array(snapshots.length).fill(null), client: p.client };
                    compData[p.ip].data[i] = p.progress * 100;
                }
                // Mark completion
                if (p.progress >= 1.0 && compData[p.ip]) {
                    compData[p.ip].data[i] = 100;
                }
            }
        } catch(_) {}
    }

    // Top 5 competitors by max progress
    const compEntries = Object.entries(compData)
        .sort((a, b) => Math.max(...(b[1].data.filter(v=>v!==null))) - Math.max(...(a[1].data.filter(v=>v!==null))))
        .slice(0, 5);

    const datasets = [
        {
            label: "Us", data: ourProgress,
            borderColor: "#58a6ff", backgroundColor: "rgba(88,166,255,0.1)",
            borderWidth: 2, fill: false, tension: 0.2, pointRadius: 0, yAxisID: "y",
        },
        {
            label: t("DL Rate"), data: ourDLRate,
            borderColor: "#3fb950", backgroundColor: "rgba(63,185,80,0.08)",
            borderWidth: 1, fill: true, tension: 0.2, pointRadius: 0, yAxisID: "y1",
        },
    ];

    compEntries.forEach(([ip, info], idx) => {
        datasets.push({
            label: incoIP(ip.split(":")[0]), data: info.data,
            borderColor: _tlPeerColors[(idx + 2) % _tlPeerColors.length],
            borderWidth: 1.5, borderDash: [4, 2], fill: false,
            tension: 0.2, pointRadius: 0, yAxisID: "y", spanGaps: true,
        });
    });

    // Event annotations as point markers on our progress line
    const eventPoints = [];
    for (const ev of events) {
        if (ev.event === "added" || ev.event === "announce") continue;
        const idx = snapshots.findIndex(s => s.ts >= ev.ts);
        if (idx >= 0) eventPoints.push({ idx, event: ev.event });
    }

    _tlProgressChart = new Chart(canvas, {
        type: "line",
        data: { labels, datasets },
        options: {
            responsive: true, maintainAspectRatio: false, animation: false,
            interaction: { mode: "index", intersect: false },
            plugins: {
                legend: { display: true, labels: { color: "#7a90a8", boxWidth: 10, font: { size: 10 } } },
                tooltip: {
                    mode: "index", intersect: false,
                    callbacks: {
                        label: function(ctx) {
                            if (ctx.dataset.yAxisID === "y1") return `DL: ${formatBytes(ctx.raw)}/s`;
                            return `${ctx.dataset.label}: ${ctx.raw !== null ? ctx.raw.toFixed(1) + "%" : "-"}`;
                        }
                    }
                },
            },
            scales: {
                x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0, font: { size: 9 } }, grid: { display: false } },
                y: { display: true, min: 0, max: 100, ticks: { color: "#7a90a8", callback: v => v + "%", font: { size: 9 } }, grid: { color: "#1a2233" } },
                y1: { display: true, position: "right", min: 0, ticks: { color: "#3fb95088", callback: v => formatBytes(v) + "/s", font: { size: 9 } }, grid: { display: false } },
            },
            onClick: (evt, items) => {
                if (items.length > 0) {
                    const idx = items[0].index;
                    if (idx < snapshots.length) renderTimelinePeers(snapshots[idx], t0);
                }
            },
        },
    });
}

function renderPeerSpeedChart(snapshots, t0) {
    if (_tlPeerChart) { _tlPeerChart.destroy(); _tlPeerChart = null; }
    const canvas = document.getElementById("tl-peer-chart");
    if (!canvas || snapshots.length === 0) return;

    const labels = snapshots.map(s => formatDuration(s.ts - t0));

    // Collect all unique peers and their DL speed over time
    const peerSpeeds = {};
    for (let i = 0; i < snapshots.length; i++) {
        const snap = snapshots[i];
        if (!snap.peers_json || snap.peers_json === "[]") continue;
        try {
            const peers = JSON.parse(snap.peers_json);
            for (const p of peers) {
                const key = p.ip;
                if (!peerSpeeds[key]) peerSpeeds[key] = { data: new Array(snapshots.length).fill(0), client: p.client, maxSpeed: 0 };
                peerSpeeds[key].data[i] = p.dl_speed || 0;
                if (p.dl_speed > peerSpeeds[key].maxSpeed) peerSpeeds[key].maxSpeed = p.dl_speed;
            }
        } catch(_) {}
    }

    // Top 8 peers by max DL speed
    const topPeers = Object.entries(peerSpeeds)
        .sort((a, b) => b[1].maxSpeed - a[1].maxSpeed)
        .slice(0, 8);

    const datasets = topPeers.map(([ip, info], idx) => ({
        label: incoIP(ip.split(":")[0]),
        data: info.data,
        borderColor: _tlPeerColors[idx % _tlPeerColors.length],
        backgroundColor: _tlPeerColors[idx % _tlPeerColors.length] + "33",
        borderWidth: 1, fill: true, tension: 0.2, pointRadius: 0,
    }));

    _tlPeerChart = new Chart(canvas, {
        type: "line",
        data: { labels, datasets },
        options: {
            responsive: true, maintainAspectRatio: false, animation: false,
            interaction: { mode: "index", intersect: false },
            plugins: {
                legend: { display: true, labels: { color: "#7a90a8", boxWidth: 10, font: { size: 10 } } },
                tooltip: {
                    mode: "index", intersect: false,
                    callbacks: { label: ctx => `${ctx.dataset.label}: ${formatBytes(ctx.raw)}/s` }
                },
            },
            scales: {
                x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0, font: { size: 9 } }, grid: { display: false } },
                y: { display: true, min: 0, ticks: { color: "#7a90a8", callback: v => formatBytes(v) + "/s", font: { size: 9 } }, grid: { color: "#1a2233" }, stacked: true },
            },
            onClick: (evt, items) => {
                if (items.length > 0) {
                    const idx = items[0].index;
                    if (idx < snapshots.length) renderTimelinePeers(snapshots[idx], t0);
                }
            },
        },
    });
}

function renderTimelinePeers(snapshot, t0) {
    document.getElementById("tl-peer-time").textContent = `T+${formatDuration(snapshot.ts - t0)}`;
    const tbody = document.getElementById("tl-peer-tbody");
    if (!snapshot.peers_json || snapshot.peers_json === "[]") {
        tbody.innerHTML = '<tr><td colspan="5" class="empty">No peer data</td></tr>';
        return;
    }
    try {
        const peers = JSON.parse(snapshot.peers_json);
        peers.sort((a, b) => (b.dl_speed || 0) - (a.dl_speed || 0));
        tbody.innerHTML = peers.map(p => {
            const prog = p.progress >= 1.0
                ? '<span style="color:#3fb950">100%</span>'
                : `${(p.progress * 100).toFixed(0)}%`;
            return `<tr>
                <td class="mono" style="font-size:10px">${incoIP(p.ip)}</td>
                <td style="font-size:10px">${p.client || "?"}</td>
                <td>${formatBytes(p.dl_speed || 0)}/s</td>
                <td>${prog}</td>
                <td style="color:#3fb950;font-size:10px">${p.flags || ""}</td>
            </tr>`;
        }).join("");
    } catch(_) {
        tbody.innerHTML = '<tr><td colspan="5" class="empty">Parse error</td></tr>';
    }
}

function renderTimelineEvents(events, t0) {
    const countEl = document.getElementById("tl-events-count");
    countEl.textContent = `(${events.length})`;
    const container = document.getElementById("tl-events-log");

    if (events.length === 0) {
        container.innerHTML = '<div class="empty">No events</div>';
        return;
    }

    const iconMap = { added: "+", first_peer: "P", first_upload: "U", completed: "",
        uploader_injected: "\u21e8", injection_hit: "\u2605", announce: "A" };
    const hlEvents = new Set(["completed"]);

    container.innerHTML = events.map(ev => {
        const tPlus = formatDuration(ev.ts - t0);
        const isHl = hlEvents.has(ev.event);
        const icon = iconMap[ev.event] || "?";
        let detail = `${ev.peers}p ${ev.swarm_seeds}s/${ev.swarm_leechers}l`;
        if (ev.event === "completed" && ev.download_time > 0) detail = `${formatDuration(ev.download_time)}`;
        if (ev.event === "uploader_injected") detail = ev.uploader + " " + t("({n} peers)", { n: ev.injected_peers });
        if (ev.event === "announce") detail = `${ev.swarm_seeds}s/${ev.swarm_leechers}l`;
        return `<div class="tl-ev-row${isHl ? " hl" : ""}">
            <span class="tl-ev-t">${tPlus}</span>
            <span style="min-width:14px;text-align:center;font-size:10px">${icon}</span>
            <span class="tl-ev-txt"><strong>${ev.event.replace(/_/g," ")}</strong> <span class="d">${detail}</span></span>
        </div>`;
    }).join("");
}

function formatDate(ts) {
    if (!ts || ts <= 0) return "-";
    const d = new Date(ts * 1000);
    return d.toLocaleDateString() + " " + d.toLocaleTimeString([], {hour:"2-digit", minute:"2-digit"});
}

function formatDuration(seconds) {
    if (!seconds || seconds <= 0) return "-";
    const h = Math.floor(seconds / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    const s = Math.floor(seconds % 60);
    if (h > 0) return `${h}h ${m}m ${s}s`;
    if (m > 0) return `${m}m ${s}s`;
    return `${s}s`;
}

// displayRatio mirrors the torrent-list ratio (total_upload / bytes-we-have)
// instead of the engine's raw upload/download. For our own uploads we never
// downloaded, so download==0 -> engine ratio is 0; measuring against the data
// we actually hold (done = total_done, or total_size*progress as fallback)
// gives the same meaningful ratio the table shows.
function displayRatio(d) {
    let done = (d.total_done && d.total_done > 0)
        ? d.total_done
        : (d.total_size > 0 ? d.total_size * (d.progress || 0) : 0);
    if (done > 0) return d.total_upload / done;
    return d.ratio || 0;
}

async function refreshDetail() {
    if (!selectedTorrent) return;
    try {
        const q = selectedTorrentAgent && !_isLocalAgent(selectedTorrentAgent)
            ? `?agent=${encodeURIComponent(selectedTorrentAgent)}` : "";
        const d = await api(`/api/race/torrents/${selectedTorrent}${q}`);

        _detailAddedTime = d.added_time || 0;
        document.getElementById("detail-name").textContent = incoName(d);
        document.getElementById("detail-state").textContent = d.state;
        document.getElementById("detail-progress").textContent = (d.progress * 100).toFixed(1) + "%";
        document.getElementById("detail-size").textContent = formatBytes(d.total_size);
        document.getElementById("detail-downloaded").textContent = formatBytes(d.total_download);
        document.getElementById("detail-uploaded").textContent = formatBytes(d.total_upload);
        document.getElementById("detail-ratio").textContent = displayRatio(d).toFixed(4);
        document.getElementById("detail-path").textContent = incoPath(d.save_path);
        document.getElementById("detail-hash").textContent = d.info_hash;
        renderPieceMap(d.pieces_have, d.pieces_avail, "detail-pieces-canvas", "detail-pieces-info");
        document.getElementById("detail-dl-speed").textContent = formatSpeed(d.download_rate);
        document.getElementById("detail-ul-speed").textContent = formatSpeed(d.upload_rate);
        document.getElementById("detail-avg-dl").textContent = formatSpeed(d.avg_download_rate || 0);
        document.getElementById("detail-avg-ul").textContent = formatSpeed(d.avg_upload_rate || 0);
        document.getElementById("detail-peers-count").textContent = d.num_peers;
        document.getElementById("detail-seeds-count").textContent = d.num_seeds;
        document.getElementById("detail-swarm-seeds").textContent = d.swarm_seeds || 0;
        document.getElementById("detail-swarm-leechers").textContent = d.swarm_leechers || 0;
        document.getElementById("detail-pieces").textContent = d.num_pieces;
        document.getElementById("detail-piece-size").textContent = formatBytes(d.piece_length);
        document.getElementById("detail-active-time").textContent = formatDuration(d.active_time);
        document.getElementById("detail-seeding-time").textContent = formatDuration(d.seeding_time);

        // Choking
        document.getElementById("detail-choking-scored").textContent = d.choking?.num_scored ?? "-";
        document.getElementById("detail-choking-unchoked").textContent = d.choking?.num_unchoked ?? "-";

        // Peers table
        const ptbody = document.getElementById("detail-peers-tbody");
        if (d.peers && d.peers.length > 0) {
            ptbody.innerHTML = d.peers.map(p => {
                const flags = (p.flags || []).map(f =>
                    `<span class="peer-flag ${f}">${f}</span>`
                ).join("");
                return `<tr>
                    <td>${incoIP(p.ip)}:${p.port}</td>
                    <td>${p.client || "-"}</td>
                    <td>${(p.progress * 100).toFixed(0)}%</td>
                    <td>${formatSpeed(p.down_speed)}</td>
                    <td>${formatSpeed(p.up_speed)}</td>
                    <td>${formatBytes(p.total_download)}</td>
                    <td>${formatBytes(p.total_upload)}</td>
                    <td>${flags || "-"}</td>
                </tr>`;
            }).join("");
        } else {
            ptbody.innerHTML = '<tr><td colspan="8" class="empty">No peers connected</td></tr>';
        }

        // Trackers table. Skipped while the editor is open: redrawing the
        // list someone is editing is how an edit gets lost mid-typing.
        const ttbody = document.getElementById("detail-trackers-tbody");
        if (trackerEditorIsOpen()) return;
        if (d.trackers && d.trackers.length > 0) {
            ttbody.innerHTML = d.trackers.map(t => {
                let domain = t.url;
                try { domain = new URL(t.url).hostname; } catch (_) {}
                const ep = t.endpoints && t.endpoints[0];
                const hasErr = ep && ep.last_error && ep.last_error !== "Success";
                const msg = ep ? (ep.message || ep.last_error || "") : "";
                const nextAnn = ep && ep.next_announce !== undefined ? ep.next_announce : -1;
                const lastAnn = ep && ep.last_announce !== undefined ? ep.last_announce : -1;
                const seeds = ep ? ep.scrape_complete : -1;
                const leechers = ep ? ep.scrape_incomplete : -1;
                const statusHtml = hasErr
                    ? `<span class="tracker-err" title="${msg}">${msg.substring(0, 60) || "error"}</span>`
                    : `<span class="tracker-ok">${msg || "OK"}</span>`;
                const nextStr = nextAnn > 0 ? `${Math.floor(nextAnn/60)}m${nextAnn%60}s` : nextAnn === 0 ? "now" : "-";
                const scrapeStr = seeds >= 0 ? `${seeds}s/${leechers}l` : "";
                return `<tr><td class="mono" title="${t.url}">${domain}</td><td>${statusHtml}</td><td class="mono">${formatAgo(lastAnn)}</td><td class="mono">${nextStr}</td><td class="mono">${scrapeStr}</td></tr>`;
            }).join("");
        } else {
            ttbody.innerHTML = '<tr><td colspan="5" class="empty">No trackers</td></tr>';
        }
    } catch (e) {
        console.error("Failed to refresh detail:", e);
    }
}

// ─── Tracker editing ────────────────────────────────────
//
// The trackers table refreshes on a timer, so it holds no controls: a button
// that moves under the pointer is worse than no button. Editing happens in a
// panel that suspends the refresh while it is open, and the whole list is sent
// at once -- add, remove and rename are all "edit the text and save".
//
// One URL per line, a blank line starts a new tier, which is the convention
// qBittorrent's editor uses. Tiers are tried in order, so this is not
// decoration: flattening them would change which tracker answers first.

// Announce timings come as seconds, never absolute stamps: rendering a
// server timestamp against the browser clock shows nonsense whenever the two
// disagree. -1 means the engine has no answer (never announced, or a tracker
// it has not reached yet) and reads as a dash, which is not the same claim as
// "0 seconds ago".
function formatAgo(secs) {
    if (secs === undefined || secs === null || secs < 0) return "-";
    if (secs < 60) return secs + "s " + t("ago");
    if (secs < 3600) return Math.floor(secs / 60) + "m " + t("ago");
    if (secs < 86400) return Math.floor(secs / 3600) + "h " + t("ago");
    return Math.floor(secs / 86400) + "d " + t("ago");
}

let _trkEditHash = null;
let _trkEditEngine = null;

function trackersToText(tiers) {
    return (tiers || []).map(tier => (tier || []).join("\n")).join("\n\n");
}

function textToTiers(text) {
    const tiers = [];
    let cur = [];
    for (const rawLine of String(text).split("\n")) {
        const line = rawLine.trim();
        if (!line) {
            if (cur.length) { tiers.push(cur); cur = []; }
            continue;
        }
        cur.push(line);
    }
    if (cur.length) tiers.push(cur);
    return tiers;
}

async function openTrackerEditor(infoHash) {
    _trkEditHash = infoHash;
    const box = document.getElementById("trk-edit-modal");
    const ta = document.getElementById("trk-edit-text");
    const out = document.getElementById("trk-edit-result");
    out.textContent = "";
    out.className = "trk-edit-msg";
    ta.value = t("Loading...");
    ta.disabled = true;
    box.style.display = "flex";
    try {
        const res = await api(`/api/torrents/${infoHash}/trackers`);
        _trkEditEngine = res.engine || "";
        ta.value = trackersToText(res.trackers);
        ta.disabled = false;
        ta.focus();
        const eng = document.getElementById("trk-edit-engine");
        if (eng) eng.textContent = _trkEditEngine ? t("Engine") + ": " + _trkEditEngine : "";
    } catch (e) {
        ta.value = "";
        ta.disabled = true;
        out.textContent = t("Could not read the tracker list") + ": " + e.message;
        out.className = "trk-edit-msg error";
    }
}

function closeTrackerEditor() {
    document.getElementById("trk-edit-modal").style.display = "none";
    _trkEditHash = null;
}

// The editor is open = the detail refresh must not redraw underneath it.
function trackerEditorIsOpen() {
    const box = document.getElementById("trk-edit-modal");
    return !!box && box.style.display === "flex";
}

async function saveTrackerEditor() {
    if (!_trkEditHash) return;
    const out = document.getElementById("trk-edit-result");
    const tiers = textToTiers(document.getElementById("trk-edit-text").value);
    out.className = "trk-edit-msg";
    out.textContent = t("Saving...");
    try {
        const res = await api(`/api/torrents/${_trkEditHash}/trackers`, {
            method: "POST",
            body: JSON.stringify({ op: "set", tiers }),
        });
        if (res.changed === false) {
            out.textContent = t("No change to save.");
            return;
        }
        const n = (res.trackers || []).reduce((a, tr) => a + tr.length, 0);
        // Say what actually happened, including the part that is not obvious:
        // the change is live now AND written to the stored .torrent.
        out.textContent = t("Saved. Announcing to") + " " + n + " " + t("tracker(s) from the next announce, and kept across restarts.");
        closeTrackerEditor();
        if (typeof refreshDetail === "function") refreshDetail();
    } catch (e) {
        out.textContent = e.message;
        out.className = "trk-edit-msg error";
    }
}

// ─── Hoard stats ────────────────────────────────────────

function setHoardStateFilter(el, value) {
    _hoardStateFilter = value;
    document.querySelectorAll(".chip-state").forEach(c => c.classList.toggle("active", c === el));
    renderHoardTable();
    _renderHoardCounts();
}

function setHoardCatFilter(el, value) {
    _hoardCatFilter = _hoardCatFilter === value ? "" : value;
    document.querySelectorAll(".chip-cat").forEach(c => c.classList.toggle("active", c.dataset.cat === _hoardCatFilter && _hoardCatFilter !== ""));
    renderHoardTable();
    _renderHoardCounts();
}

function setHoardTrackerFilter(el, value) {
    _hoardTrackerFilter = _hoardTrackerFilter === value ? "" : value;
    document.querySelectorAll(".chip-tracker").forEach(c => c.classList.toggle("active", c.dataset.tracker === _hoardTrackerFilter && _hoardTrackerFilter !== ""));
    renderHoardTable();
    _renderHoardCounts();
}

function setHoardTagFilter(el, value) {
    _hoardTagFilter = _hoardTagFilter === value ? "" : value;
    document.querySelectorAll(".chip-tag").forEach(c => c.classList.toggle("active", c.dataset.tag === _hoardTagFilter && _hoardTagFilter !== ""));
    renderHoardTable();
    _renderHoardCounts();
}

// --- Tags context-menu editor (hoard-only, multi-select) ---
async function _showTagPicker(ev) {
    if (ev) ev.stopPropagation();
    const anchor = ev && ev.currentTarget;
    const hoardOnly = [..._selected.entries()].filter(([, v]) => _selMode(v) === "hoard");
    if (hoardOnly.length === 0) return;
    let known = [];
    try { known = await api("/api/tags"); } catch (e) { known = []; }
    const firstRow = _hoardAllTorrents.find(t => t.info_hash === hoardOnly[0][0]);
    const cur = new Set((firstRow && firstRow.tags) || []);
    const esch = s => String(s).replace(/[&<>"']/g, ch => ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#39;"}[ch]));
    const rows = known.map(t => {
        const jsT = String(t).replace(/\\/g, "\\\\").replace(/'/g, "\\'");
        const on = cur.has(t);
        return `<div class="ctx-item" onclick="_toggleTagSelected('${jsT}', ${on ? "false" : "true"})">${on ? "✓ " : " "}${esch(t)}</div>`;
    }).join("");
    const label = tp(hoardOnly.length, "Edit tags, {n} torrent", "Edit tags, {n} torrents");
    _openCtxSubmenu(
        `<div class="ctx-label">${label}</div>` +
        `<div class="ctx-separator"></div>` +
        `<div style="padding:6px 10px"><input type="text" id="ctx-new-tag" placeholder="${t("new tag + Enter")}" style="width:100%" onclick="event.stopPropagation()" onkeydown="if(event.key==='Enter'){event.preventDefault();_addNewTagSelected();}"></div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-scroll">${rows || '<div class="ctx-item" style="opacity:.6">No tags yet</div>'}</div>`, anchor);
}

async function _applyTagOp(tags, op) {
    const entries = [..._selected.entries()].filter(([, v]) => _selMode(v) === "hoard");
    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        try {
            await fetch(`/api/hoard/torrents/${hash}/tags`, {
                method: "POST",
                headers: { "X-Api-Key": API_KEY, "Content-Type": "application/json" },
                body: JSON.stringify({ tags, op }),
            });
            const row = _hoardAllTorrents.find(t => t.info_hash === hash);
            if (row) {
                const set = new Set(row.tags || []);
                if (op === "add") tags.forEach(t => set.add(t));
                else tags.forEach(t => set.delete(t));
                row.tags = [...set];
            }
        } catch (e) { console.error("tag op failed", hash, e); }
    }
    renderHoardTable();
    _renderHoardCounts();
}

function _toggleTagSelected(tag, add) {
    _applyTagOp([tag], add ? "add" : "remove").then(() => _showTagPicker(null));
}

function _addNewTagSelected() {
    const el = document.getElementById("ctx-new-tag");
    const val = ((el && el.value) || "").trim();
    if (!val) return;
    _applyTagOp([val], "add").then(() => _showTagPicker(null));
}

function sortHoard(th) {
    const col = th.dataset.col;
    if (_hoardSortCol === col) {
        _hoardSortAsc = !_hoardSortAsc;
    } else {
        _hoardSortCol = col;
        _hoardSortAsc = true;
    }
    localStorage.setItem("hydra_hoard_sort_col", _hoardSortCol);
    localStorage.setItem("hydra_hoard_sort_asc", _hoardSortAsc ? "1" : "0");
    document.querySelectorAll("#hoard-table thead th").forEach(h => {
        h.classList.remove("sort-asc", "sort-desc");
    });
    th.classList.add(_hoardSortAsc ? "sort-asc" : "sort-desc");
    renderHoardTable();
}

function renderHoardTable() {
    const search = (document.getElementById("hoard-search")?.value || "").toLowerCase();
    const catFilter = _hoardCatFilter;
    const stateFilter = _hoardStateFilter;

    let filtered = _hoardAllTorrents.filter(t => _hoardMatches(t, search, null));

    // Tri
    const col = _hoardSortCol;
    const asc = _hoardSortAsc;
    filtered = [...filtered].sort((a, b) => {
        let va = a[col] ?? 0, vb = b[col] ?? 0;
        if (typeof va === "string") {
            const cmp = va.localeCompare(vb);
            if (cmp !== 0) return asc ? cmp : -cmp;
        } else {
            const diff = asc ? va - vb : vb - va;
            if (diff !== 0) return diff;
        }
        return (a.info_hash || "").localeCompare(b.info_hash || "");
    });

    // Keep the filtered set around: only HOARD_RENDER_LIMIT rows are rendered,
    // so Ctrl+A cannot read the selection universe off the DOM.
    _hoardFiltered = filtered;
    const visible = filtered.slice(0, HOARD_RENDER_LIMIT);
    const countEl = document.getElementById("hoard-filter-count");
    if (countEl) {
        if (filtered.length > HOARD_RENDER_LIMIT)
            countEl.textContent = t("{shown} / {matched} ({total} total)", { shown: HOARD_RENDER_LIMIT, matched: filtered.length, total: _hoardAllTorrents.length });
        else
            countEl.textContent = `${filtered.length} / ${_hoardAllTorrents.length}`;
    }

    const tbody = document.getElementById("hoard-tbody");
    if (!visible.length) {
        tbody.innerHTML = `<tr><td colspan="${_visibleCols("hoard-table").length}" class="empty">No hoard torrents</td></tr>`;
        return;
    }
    tbody.innerHTML = visible.map(t => {
        const detailSel = selectedHoardTorrent === t.info_hash ? " selected" : "";
        return `<tr class="t-row clickable${detailSel}" data-hash="${t.info_hash}" data-mode="hoard" data-agent="${t.agent || 'local'}" onclick="handleRowClick(event,'${t.info_hash}','hoard')" oncontextmenu="handleRowContextMenu(event,'${t.info_hash}','hoard')">${renderRowCells("hoard-table", t)}</tr>`;
    }).join("");
    _updateRowHighlights();
}

// Recompute state/cat chip counts. Called only when the torrent list mutates
// (30s backstop fetch, torrent_added/removed), not on each stats snapshot.
function _renderHoardCounts() {
    if (!_hoardAllTorrents) return;
    // Faceted: every group is counted with the OTHER groups applied, never its
    // own. Selecting a category makes each tracker report what it holds inside
    // that category, while the tracker list itself stays whole and switchable.
    const search = (document.getElementById("hoard-search")?.value || "").toLowerCase();
    const forState = _hoardAllTorrents.filter(t => _hoardMatches(t, search, "state"));
    const forCat = _hoardAllTorrents.filter(t => _hoardMatches(t, search, "cat"));
    const forTrk = _hoardAllTorrents.filter(t => _hoardMatches(t, search, "tracker"));
    const forTag = _hoardAllTorrents.filter(t => _hoardMatches(t, search, "tag"));

    const stateCounts = {};
    forState.forEach(t => { stateCounts[t.state] = (stateCounts[t.state] || 0) + 1; });
    const nAll = forState.length;
    const nActive = forState.filter(t => t.state === "seeding" && t.upload_rate > 0).length;
    const nTrackerErr = forState.filter(t => t.tracker_error).length;
    const nTorrentErr = forState.filter(t => t.torrent_error).length;
    const nPinned = forState.filter(t => _hoardPinned.has(t.info_hash)).length;
    document.querySelector(".chip-state[data-state='']").innerHTML = `All <span class="chip-count">${nAll}</span>`;
    document.querySelector(".chip-state[data-state='seeding']").innerHTML = `Seeding <span class="chip-count">${stateCounts["seeding"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='__active__']").innerHTML = `Actively Seeding <span class="chip-count">${nActive}</span>`;
    document.querySelector(".chip-state[data-state='downloading']").innerHTML = `Downloading <span class="chip-count">${stateCounts["downloading"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='__pinned__']").innerHTML = `Forced <span class="chip-count">${nPinned}</span>`;
    // Stopped is the user's doing, Queued is a scheduler's. Both are halted,
    // and telling them apart is the whole point of the two chips.
    document.querySelector(".chip-state[data-state='stopped']").innerHTML = `Stopped <span class="chip-count">${stateCounts["stopped"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='queued']").innerHTML = `Queued <span class="chip-count">${stateCounts["queued"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='checking_files']").innerHTML = `Checking <span class="chip-count">${stateCounts["checking_files"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='__tracker_err__']").innerHTML = `Tracker Error <span class="chip-count">${nTrackerErr}</span>`;
    document.querySelector(".chip-state[data-state='__error__']").innerHTML = `Error <span class="chip-count">${nTorrentErr}</span>`;

    const catCounts = {};
    forCat.forEach(t => {
        const c = t.category || "";
        if (c) catCounts[c] = (catCounts[c] || 0) + 1;
    });
    const cats = Object.keys(catCounts).sort();
    const nUncat = forCat.filter(t => !(t.category)).length;
    const container = document.getElementById("hoard-cat-chips");
    if (container) {
        let html = cats.map(c =>
            `<button class="chip chip-cat${c === _hoardCatFilter ? " active" : ""}" data-cat="${c}" onclick="setHoardCatFilter(this,'${c}')">${esc(incoCat(c))} <span class="chip-count">${catCounts[c]}</span></button>`
        ).join("");
        // Meta-filter: only meaningful when at least one real category exists and
        // some torrents lack one (e.g. after a category was deleted).
        if (cats.length > 0 && nUncat > 0) {
            html = `<button class="chip chip-cat chip-none${_hoardCatFilter === "__none__" ? " active" : ""}" data-cat="__none__" style="font-style:italic;opacity:.85" onclick="setHoardCatFilter(this,'__none__')">Uncategorized <span class="chip-count">${nUncat}</span></button>` + html;
        }
        container.innerHTML = html;
    }

    const trkCounts = {};
    forTrk.forEach(t => { const h = t.tracker_host || ""; if (h) trkCounts[h] = (trkCounts[h] || 0) + 1; });
    const trks = Object.keys(trkCounts).sort();
    const trkContainer = document.getElementById("hoard-tracker-chips");
    if (trkContainer) {
        trkContainer.innerHTML = trks.map(h =>
            `<button class="chip chip-tracker${h === _hoardTrackerFilter ? " active" : ""}" data-tracker="${esc(h)}" onclick="setHoardTrackerFilter(this,'${h}')">${esc(h)} <span class="chip-count">${trkCounts[h]}</span></button>`
        ).join("");
    }

    const tagCounts = {};
    forTag.forEach(t => { (t.tags || []).forEach(tg => { tagCounts[tg] = (tagCounts[tg] || 0) + 1; }); });
    const tagNames = Object.keys(tagCounts).sort();
    const nUntagged = forTag.filter(t => !(t.tags && t.tags.length)).length;
    const tagContainer = document.getElementById("hoard-tag-chips");
    if (tagContainer) {
        let html = tagNames.map(tg =>
            `<button class="chip chip-tag${tg === _hoardTagFilter ? " active" : ""}" data-tag="${esc(tg)}" onclick="setHoardTagFilter(this,'${tg}')">${esc(tg)} <span class="chip-count">${tagCounts[tg]}</span></button>`
        ).join("");
        // Untagged meta-filter: only when tags are actually in use.
        if (tagNames.length > 0 && nUntagged > 0) {
            html = `<button class="chip chip-tag chip-none${_hoardTagFilter === "__none__" ? " active" : ""}" data-tag="__none__" style="font-style:italic;opacity:.85" onclick="setHoardTagFilter(this,'__none__')">Untagged <span class="chip-count">${nUntagged}</span></button>` + html;
        }
        tagContainer.innerHTML = html;
    }

    document.querySelectorAll("#hoard-table thead th").forEach(h => {
        if (h.dataset.col === _hoardSortCol) h.classList.add(_hoardSortAsc ? "sort-asc" : "sort-desc");
    });
}

// Render the hoard header bars + summary text from a /api/hoard/stats payload.
// Called by SSE hoard_stats_snapshot and the legacy fetch path.
function _renderHoardStatsHeader(data) {
    if (!data) return;
    const total = data.total_torrents || 1;
        document.getElementById("hoard-bar-uploading").style.width = (data.torrents_uploading / total * 100).toFixed(1) + "%";
        document.getElementById("hoard-bar-with-peers").style.width = (data.torrents_with_peers / total * 100).toFixed(1) + "%";
        const announced = data.torrents_announced ?? data.total_torrents;
        const annPct = data.total_torrents ? Math.round(announced / data.total_torrents * 100) : 100;
        const annText = annPct >= 100 ? t("all announced") : t("{done}/{total} announced ({pct}%)", { done: announced, total: data.total_torrents, pct: annPct });

        // Peer efficiency: connected peers vs available swarm leechers
        const swarm = data.swarm_leechers || 0;
        const peers = data.unseeded_peers ?? data.active_peers ?? 0;
        let peerText = t("{n} peers", { n: peers });
        if (swarm > 0) {
            const peerPct = (peers / swarm * 100).toFixed(1);
            peerText = t("{n}/{swarm} peers ({pct}%)", { n: peers, swarm: formatCount(swarm), pct: peerPct });
        }

    document.getElementById("hoard-summary-text").textContent =
        t("{total} torrents: {up} uploading, {withPeers} with peers, {peers}. {announced}", {
            total: data.total_torrents, up: data.torrents_uploading,
            withPeers: data.torrents_with_peers, peers: peerText, announced: annText });
}

// Called on tab activation, hash change, or page load. Live updates flow
// through SSE (status_snapshot + hoard_stats_snapshot + stats_snapshot) so
// the periodic poll does NOT call this anymore, it would duplicate the SSE
// stream over HTTP, which was the original bug we set out to kill.
//
// Two responsibilities:
//   1. Fire an immediate /api/hoard/stats fetch only if SSE hasn't painted
//      the header yet (cold tab activation).
//   2. Refresh the static-ish torrent list (/api/hoard/torrents) at most
//      every HOARD_FETCH_INTERVAL, backstop for name / category / scrape.
async function updateHoardStats() {
    try {
        // The hoard torrent list is now hydrated + kept live entirely over SSE
        // (torrent_batch on connect, then torrent_added/removed + stats_snapshot).
        // Here we only ensure the stats HEADER is painted on a cold tab open
        // before the first SSE snapshot lands.
        if (!_hoardStatsPainted) {
            const stats = await api("/api/hoard/stats");
            _renderHoardStatsHeader(stats);
            _hoardStatsPainted = true;
        }
    } catch (e) {
        console.error("Failed to update hoard stats:", e);
    }
}

async function showHoardDetail(infoHash, agent) {
    selectedHoardTorrent = infoHash;
    selectedHoardTorrentAgent = agent || "local";
    switchHoardDetailTab("info");
    document.getElementById("hoard-detail-panel").style.display = "block";
    if (hoardDetailTimer) clearInterval(hoardDetailTimer);
    await refreshHoardDetail();
    hoardDetailTimer = setInterval(refreshHoardDetail, 3000);
}

function closeHoardDetail() {
    selectedHoardTorrent = null;
    selectedHoardTorrentAgent = "local";
    if (hoardDetailTimer) clearInterval(hoardDetailTimer);
    const panel = document.getElementById("hoard-detail-panel");
    if (panel.style.display === "none") return;
    panel.classList.add("closing");
    panel.addEventListener("animationend", () => {
        panel.style.display = "none";
        panel.classList.remove("closing");
    }, { once: true });
}

// ─── Multi-selection + Context menu ─────────────────────

// hash -> {mode, agent}. The node matters: the same hash can be held by this
// node and by an agent at once, and an action has to reach the one clicked.
const _selected = new Map();

function _selMode(v) { return (v && v.mode !== undefined) ? v.mode : v; }
// _isLocalAgent mirrors isLocalAgentName on the server: this node answers to
// the bare "local" and to every "local-" name. The list rows carry their real
// agent since 3.146.0 -- local-race, local-hoard, local-<id> -- and every place
// that used to compare against the literal "local" would otherwise treat its
// own torrents as somebody else's: live stats skipped, removals ignored, a
// per-row action dialled at an agent that was never registered.
function _isLocalAgent(name) {
    if (!name) return true;
    return name === "local" || name.indexOf("local-") === 0;
}
function _selAgent(v) { return (v && v.agent) ? v.agent : "local"; }
function _selHash(v) { return (v && v.hash) ? v.hash : v; }

// Key of a selection entry: the row, never the hash on its own.
function _selKeyOf(hash, agent) { return _agentKey(agent) + "|" + hash; }
function _rowSelKey(row) { return _selKeyOf(row.dataset.hash, row.dataset.agent); }
function _selEntry(row) {
    return { hash: row.dataset.hash, mode: row.dataset.mode, agent: row.dataset.agent || "local" };
}
let _anchorHash = null;      // last click without shift (range start point)
let _hoardFiltered = [];     // last filtered hoard set (see renderHoardTable)

// Pinned ("force download") hashes. Held client-side rather than carried on
// every row: the list streams over SSE at six figures of torrents, and pins are
// a few hundred at most, so a per-row boolean would cost the hot path far more
// than this set costs to fetch.
let _hoardPinned = new Set();

async function _refreshHoardPins() {
    try {
        const r = await api("/api/hoard/pinned");
        _hoardPinned = new Set((r && r.pinned) || []);
    } catch (e) {
        console.warn("Failed to load pins", e);
    }
}

// One definition of what each filter group means, shared by the table and the
// chip counts so the two can never drift. `skip` names a group to ignore, which
// is what makes the counts faceted: a group must not shrink its own numbers, or
// picking one tracker would zero every other tracker and trap you there.
function _hoardMatches(t, search, skip) {
    if (skip !== "search" && search && !(t.name || t.info_hash).toLowerCase().includes(search)) return false;
    if (skip !== "cat") {
        if (_hoardCatFilter === "__none__") { if (t.category) return false; }
        else if (_hoardCatFilter && (t.category || "") !== _hoardCatFilter) return false;
    }
    if (skip !== "tracker" && _hoardTrackerFilter && (t.tracker_host || "") !== _hoardTrackerFilter) return false;
    if (skip !== "tag") {
        if (_hoardTagFilter === "__none__") { if (t.tags && t.tags.length) return false; }
        else if (_hoardTagFilter && !(t.tags || []).includes(_hoardTagFilter)) return false;
    }
    if (skip !== "state") {
        const s = _hoardStateFilter;
        if (s === "__active__") { if (!(t.state === "seeding" && t.upload_rate > 0)) return false; }
        else if (s === "__tracker_err__") { if (!t.tracker_error) return false; }
        else if (s === "__error__") { if (!t.torrent_error) return false; }
        else if (s === "__pinned__") { if (!_hoardPinned.has(t.info_hash)) return false; }
        else if (s && t.state !== s) return false;
    }
    return true;
}

// Pin or unpin the selection. Hoard-only, and the API refuses a complete
// torrent (409) since a pin only buys a download slot.
async function _pinSelected(on) {
    _hideCtxMenu();
    const entries = [..._selected.entries()];
    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        const mode = _selMode(sel);
        if (mode !== "hoard") continue;
        try {
            const res = await fetch(`/api/hoard/torrents/${hash}/${on ? "pin" : "unpin"}`, {
                method: "POST",
                headers: { "X-Api-Key": API_KEY },
            });
            if (res.ok) {
                if (on) _hoardPinned.add(hash); else _hoardPinned.delete(hash);
            } else if (res.status === 409) {
                console.warn("Cannot force a complete torrent", hash);
            }
        } catch (err) {
            console.error("Failed to pin", hash, err);
        }
    }
    renderHoardTable();
    _renderHoardCounts();
}

// Ctrl+A selects everything the current filters match -- not just the rows on
// screen. With 100k torrents the table renders a capped slice, so selecting
// "all" off the DOM would silently mean "the first 500", which is exactly the
// bulk-action trap this avoids.
function _selectAllFiltered() {
    if (!_hoardFiltered.length) return;
    _selected.clear();
    for (const t of _hoardFiltered) {
        _selected.set(_selKeyOf(t.info_hash, t.agent), { hash: t.info_hash, mode: "hoard", agent: t.agent || "local" });
    }
    _anchorHash = null;
    _updateRowHighlights();
    _flashSelectionCount(_selected.size);
}

// The highlight only shows on rendered rows, so say out loud how many are
// actually selected -- otherwise "89k selected" looks like "500 selected".
function _flashSelectionCount(n) {
    const el = document.getElementById("hoard-filter-count");
    if (!el) return;
    const prev = el.textContent;
    el.textContent = t("{n} selected", { n: n });
    el.style.fontWeight = "bold";
    clearTimeout(_flashSelectionCount._t);
    _flashSelectionCount._t = setTimeout(() => {
        el.style.fontWeight = "";
        if (el.textContent === `${n} selected`) el.textContent = prev;
    }, 2500);
}

document.addEventListener("keydown", e => {
    if (e.key !== "a" && e.key !== "A") return;
    if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
    // Never steal Ctrl+A from a text field.
    const t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    // Only meaningful while the hoard list is the visible table.
    const tbody = document.getElementById("hoard-tbody");
    if (!tbody || !tbody.offsetParent) return;
    e.preventDefault();
    _selectAllFiltered();
});

function handleRowClick(e, hash, mode) {
    const row = e.target.closest(".t-row");
    const key = row ? _rowSelKey(row) : _selKeyOf(hash, "local");
    const entry = row ? _selEntry(row) : { hash: hash, mode: mode, agent: "local" };
    if (e.shiftKey && _anchorHash) {
        // Range selection: all .t-row between the anchor and the clicked row
        const rows = [...document.querySelectorAll(".t-row")];
        const aIdx = rows.findIndex(r => _rowSelKey(r) === _anchorHash);
        const bIdx = rows.findIndex(r => _rowSelKey(r) === key);
        if (aIdx !== -1 && bIdx !== -1) {
            const lo = Math.min(aIdx, bIdx);
            const hi = Math.max(aIdx, bIdx);
            // Keep only the range selection (like Windows Explorer)
            _selected.clear();
            for (let i = lo; i <= hi; i++) {
                _selected.set(_rowSelKey(rows[i]), _selEntry(rows[i]));
            }
        }
        _updateRowHighlights();
        return;
    }
    if (e.ctrlKey || e.metaKey) {
        if (_selected.has(key)) _selected.delete(key);
        else _selected.set(key, entry);
        _anchorHash = key;
        _updateRowHighlights();
        return;
    }
    _selected.clear();
    _selected.set(key, entry);
    _anchorHash = key;
    _updateRowHighlights();
    requestAnimationFrame(() => {
        if (mode === "race") showDetail(hash, entry.agent);
        else showHoardDetail(hash, entry.agent);
    });
}

function handleRowContextMenu(e, hash, mode) {
    e.preventDefault();
    const row = e.target.closest(".t-row");
    const key = row ? _rowSelKey(row) : _selKeyOf(hash, "local");
    if (!_selected.has(key)) {
        _selected.clear();
        _selected.set(key, row ? _selEntry(row) : { hash: hash, mode: mode, agent: "local" });
        _updateRowHighlights();
    }
    _showCtxMenu(e.clientX, e.clientY);
}

function _updateRowHighlights() {
    document.querySelectorAll(".t-row").forEach(row => {
        row.classList.toggle("row-selected", _selected.has(_rowSelKey(row)));
    });
}

function _showCtxMenu(x, y) {
    const menu = document.getElementById("ctx-menu");
    const count = _selected.size;
    document.getElementById("ctx-label").textContent =
        tp(count, "{n} torrent selected", "{n} torrents selected");
    // Changing category works on both engines now: the category carries the
    // engine its torrents belong in, so setting one on a race torrent is how
    // it is handed to the hoard, and moving the files is a background job
    // either way. It used to be hoard-only, and the item was hidden here.
    const anyHoard = [..._selected.entries()].some(([, v]) => _selMode(v) === "hoard");
    const catItem = document.getElementById("ctx-change-category");
    if (catItem) catItem.style.display = "";
    const catMoveItem = document.getElementById("ctx-change-category-move");
    if (catMoveItem) catMoveItem.style.display = "";
    const rcItem = document.getElementById("ctx-recheck");
    if (rcItem) rcItem.style.display = anyHoard ? "" : "none";
    const tgItem = document.getElementById("ctx-edit-tags");
    if (tgItem) tgItem.style.display = anyHoard ? "" : "none";
    // Force download applies to incomplete hoard torrents only: a pin buys a
    // download slot, so it says nothing about one that has finished.
    const hoardSel = [..._selected.values()].filter(v => _selMode(v) === "hoard").map(v => _selHash(v));
    const incomplete = hoardSel.filter(h => {
        const row = _hoardAllTorrents.find(x => x.info_hash === h);
        return row && (row.progress || 0) < 1;
    });
    const pinItem = document.getElementById("ctx-pin");
    const unpinItem = document.getElementById("ctx-unpin");
    if (pinItem) pinItem.style.display = incomplete.some(h => !_hoardPinned.has(h)) ? "" : "none";
    if (unpinItem) unpinItem.style.display = hoardSel.some(h => _hoardPinned.has(h)) ? "" : "none";
    // Offer whichever of Pause/Resume the selection can actually act on. A
    // mixed selection gets both.
    const intents = [..._selected.values()].map(v => _isUserPaused(_selHash(v)));
    const anyPaused = intents.some(v => v === true || v === null);
    const anyRunning = intents.some(v => v === false || v === null);
    const pItem = document.getElementById("ctx-pause");
    if (pItem) pItem.style.display = anyRunning ? "" : "none";
    const rItem = document.getElementById("ctx-resume");
    if (rItem) rItem.style.display = anyPaused ? "" : "none";
    _applyCtxGroups();
    _hideCtxSubmenu();

    menu.style.left = x + "px";
    menu.style.top = y + "px";
    menu.style.display = "block";
    _clampCtxMenuToViewport();

    // Refresh in the background, then re-apply: the first open of a session
    // would otherwise decide on an empty list and hide the Agent group until
    // the menu was opened a second time.
    _refreshCtxAgents().then(() => {
        if (document.getElementById("ctx-menu").style.display === "block") {
            _applyCtxGroups();
            _clampCtxMenuToViewport();
        }
    });
}

// _applyCtxGroups decides which groups the current selection can act on.
function _applyCtxGroups() {
    // The Agent group only exists when there is somewhere to send a torrent.
    // On a single-engine install it is not greyed out, it is absent: an action
    // that can never apply is noise in a menu this size.
    //
    // "Somewhere" now includes this machine's other engines. One agent is one
    // engine, so sending a torrent from local-hoard to local-vpn7 is a real
    // move -- it changes which tunnel that torrent seeds through -- and it was
    // the whole point of adding engines. This used to filter on kind and hid
    // the group entirely on a node with no remote agent, which is every node
    // that runs several engines and nothing else.
    const sources = new Set([..._selected.values()].map(v => _selAgent(v)));
    const showAgent = _ctxAgents.some(a =>
        a.name && !(sources.size === 1 && sources.has(a.name)));
    const agentGrp = document.getElementById("ctx-grp-agent");
    const agentSep = document.getElementById("ctx-sep-agent");
    if (agentGrp) agentGrp.style.display = showAgent ? "" : "none";
    if (agentSep) agentSep.style.display = showAgent ? "" : "none";

    // A group whose every item is hidden must hide its heading too, or the
    // menu shows a title introducing nothing.
    document.querySelectorAll("#ctx-menu .ctx-group").forEach(grp => {
        if (grp.id === "ctx-grp-agent") return; // decided above
        const anyVisible = [...grp.querySelectorAll(".ctx-item")]
            .some(it => it.style.display !== "none");
        grp.style.display = anyVisible ? "" : "none";
    });
}

// Known agents, cached so opening the menu stays synchronous.
let _ctxAgents = [];

async function _refreshCtxAgents() {
    try {
        const list = await api("/api/agents");
        if (Array.isArray(list)) _ctxAgents = list;
    } catch (e) {
        // Leave the previous list in place: a failed poll is not evidence that
        // the agents went away, and blanking it would make the menu flicker.
    }
}

// _showAgentPicker lists the agents a selection can be sent to.
//
// mode is "duplicate" (both nodes keep it) or "move" (the source is released
// once the target is verified and running). The destination path is not asked
// for: it is what the torrent's category defines on that agent.
async function _showAgentPicker(ev, mode) {
    if (ev) ev.stopPropagation();
    const anchor = ev && ev.currentTarget;
    if (_selected.size === 0) return;
    await _refreshCtxAgents();

    const sources = new Set([..._selected.values()].map(v => _selAgent(v)));
    // An engine of this machine is a legitimate destination too: a torrent can
    // come back from an agent, or move between two local engines to leave by
    // another tunnel. Only the agent it already sits on is excluded, and only
    // when the whole selection agrees on one source.
    //
    // Duplicating onto this machine is refused, not offered and then rejected:
    // two local engines share a filesystem, so a "copy" would be the same files
    // twice -- two writers on the same bytes the first time either repairs a
    // piece. The daemon refuses it as well; this only keeps the menu honest.
    const targets = _ctxAgents
        .filter(a => a.name)
        .filter(a => !(sources.size === 1 && sources.has(a.name)))
        .filter(a => !(mode === "duplicate" && a.kind === "local" && [...sources].every(_isLocalAgent)));

    const esc = s => String(s).replace(/[&<>"\']/g, ch =>
        ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","\'":"&#39;"}[ch]));

    let items;
    if (!targets.length) {
        items = `<div class="ctx-label">${t("No other agent to send to")}</div>`;
    } else {
        items = targets.map(a => {
            const jsName = String(a.name).replace(/\\/g, "\\\\").replace(/\'/g, "\\\'");
            const off = a.online === false ? ` <span class="sr-desc">(${t("offline")})</span>` : "";
            // Say which ones cost nothing: between two engines of this machine
            // the payload never moves, the torrent changes hands where it lies.
            const here = a.kind === "local" && [...sources].every(_isLocalAgent)
                ? ` <span class="sr-desc">(${t("here, no copy")})</span>` : "";
            return `<div class="ctx-item" onclick="_sendToAgentSelected(\'${jsName}\', \'${mode}\')">${esc(a.name)}${off}${here}</div>`;
        }).join("");
    }
    const verb = mode === "move" ? t("Move to agent") : t("Duplicate to agent");
    const label = verb + ": " + tp(_selected.size, "{n} torrent", "{n} torrents");
    _openCtxSubmenu(
        `<div class="ctx-label">${label}</div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-scroll">${items}</div>`, anchor);
}

// _rowAgent is which node currently holds a torrent, straight off the row.
function _rowAgent(hash) {
    const row = document.querySelector(`.t-row[data-hash="${hash}"]`);
    return (row && row.dataset.agent) || "local";
}

async function _sendToAgentSelected(agentName, mode) {
    const entries = [..._selected.entries()];
    _hideCtxMenu();

    let queued = 0;
    const errors = [];
    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        const source = _rowAgent(hash);
        if (source === agentName) continue;
        try {
            const r = await fetch("/api/jobs/move-remote", {
                method: "POST",
                headers: { "X-Api-Key": API_KEY, "Content-Type": "application/json" },
                body: JSON.stringify({
                    info_hash: hash,
                    source_agent: source,
                    target_agent: agentName,
                    mode: mode,
                }),
            });
            if (r.ok) {
                queued++;
            } else {
                const body = await r.json().catch(() => ({}));
                errors.push(`${hash.slice(0, 8)}: ${body.error || ("HTTP " + r.status)}`);
            }
        } catch (e) {
            errors.push(`${hash.slice(0, 8)}: ${e.message}`);
        }
    }
    if (errors.length > 0) {
        hydraNotify(t("Send to agent"),
            t("Sent {ok} to \"{agent}\", {failed} failure(s).",
                { ok: queued, agent: agentName, failed: errors.length }) + "\n\n" + errors.join("\n"));
    } else if (queued > 0) {
        // Hours of transfer follow, so say where to watch it rather than
        // leaving the row looking unchanged.
        hydraNotify(mode === "move" ? t("Move to agent") : t("Duplicate to agent"), mode === "move"
            ? tp(queued,
                "Moving {n} torrent to \"{agent}\". It keeps seeding on this node until the copy is verified there; follow it in Jobs.",
                "Moving {n} torrents to \"{agent}\". They keep seeding on this node until their copies are verified there; follow them in Jobs.",
                { n: queued, agent: agentName })
            : tp(queued,
                "Duplicating {n} torrent to \"{agent}\". Both nodes will hold it; follow it in Jobs.",
                "Duplicating {n} torrents to \"{agent}\". Both nodes will hold them; follow them in Jobs.",
                { n: queued, agent: agentName }));
    }
}

// Re-position the ctx-menu so it fits inside the viewport. Called on
// initial open and after any innerHTML swap (e.g. switching to the
// category picker which is taller than the actions view).
function _clampCtxMenuToViewport() {
    const menu = document.getElementById("ctx-menu");
    requestAnimationFrame(() => {
        const r = menu.getBoundingClientRect();
        const margin = 8;
        const vw = window.innerWidth;
        const vh = window.innerHeight;
        if (r.right > vw - margin) {
            menu.style.left = Math.max(margin, vw - r.width - margin) + "px";
        }
        if (r.bottom > vh - margin) {
            menu.style.top = Math.max(margin, vh - r.height - margin) + "px";
        }
        if (r.left < margin) menu.style.left = margin + "px";
        if (r.top < margin) menu.style.top = margin + "px";
    });
}

function _hideCtxMenu() {
    document.getElementById("ctx-menu").style.display = "none";
    _hideCtxSubmenu();
}

function _hideCtxSubmenu() {
    const sub = document.getElementById("ctx-submenu");
    if (sub) sub.style.display = "none";
    _ctxSubAnchor = null;
}

// _openCtxSubmenu shows a second panel BESIDE the first, rather than replacing
// its contents.
//
// Swapping the menu in place cost the reader their bearings: the actions they
// came from vanished, and getting back needed a "Back" row that exists only
// because of the swap. A cascading panel keeps the parent on screen, so the
// choice being made stays visibly attached to the action that opened it.
// The row the open submenu belongs to. Kept so a submenu that re-renders
// itself (adding a tag re-opens the tag panel) stays where it was.
let _ctxSubAnchor = null;

function _openCtxSubmenu(html, anchor) {
    const main = document.getElementById("ctx-menu");
    const sub = document.getElementById("ctx-submenu");
    if (!main || !sub) return;
    if (anchor) _ctxSubAnchor = anchor;
    sub.innerHTML = html;
    // Measure before showing: the panel's size decides which side it opens on,
    // and a visible reflow would flash it in the wrong place first.
    sub.style.visibility = "hidden";
    sub.style.display = "block";
    sub.style.left = "0px";
    sub.style.top = "0px";
    requestAnimationFrame(() => {
        const m = main.getBoundingClientRect();
        const r = sub.getBoundingClientRect();
        const margin = 8;
        // Sit against the parent's right edge, overlapping its border by a
        // pixel so the two panels read as one object.
        let left = m.right - 1;
        if (left + r.width > window.innerWidth - margin) {
            left = m.left - r.width + 1;   // no room on the right: open left
        }
        if (left < margin) left = margin;
        // Line the panel up with the row that opened it, the way a cascading
        // menu does: aligning on the parent's top edge puts the panel nowhere
        // near the item clicked once the menu is tall. The 4px lifts it by the
        // panel's own vertical padding so the two rows read level.
        const a = _ctxSubAnchor;
        let top = m.top;
        if (a && document.contains(a)) {
            top = a.getBoundingClientRect().top - 4;
        }
        if (top < margin) top = margin;
        if (top + r.height > window.innerHeight - margin) {
            top = Math.max(margin, window.innerHeight - r.height - margin);
        }
        sub.style.left = left + "px";
        sub.style.top = top + "px";
        sub.style.visibility = "";
    });
}

async function _showCategoryPicker(ev, move) {
    if (ev) ev.stopPropagation();
    const anchor = ev && ev.currentTarget;
    if (_selected.size === 0) return;
    let cats = [];
    try {
        cats = await api("/api/categories");
    } catch (err) {
        console.error("Failed to load categories", err);
        return;
    }
    const esc = s => String(s).replace(/[&<>"\']/g, ch =>
        ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","\'":"&#39;"}[ch]));
    const items = cats.map(c => {
        const safeName = esc(c.name);
        const safePath = esc(c.save_path || "");
        // category name is embedded as a JS string literal, escape quotes.
        const jsName = String(c.name).replace(/\\/g, "\\\\").replace(/\'/g, "\\\'");
        return `<div class="ctx-item" onclick="_changeCategorySelected(\'${jsName}\', ${move ? "true" : "false"})" title="${safePath}">${safeName}</div>`;
    }).join("");
    const verb = move ? t("Move to category") : t("Set category (no move)");
    const label = verb + ": " + tp(_selected.size, "{n} torrent", "{n} torrents");
    _openCtxSubmenu(
        `<div class="ctx-label">${label}</div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-scroll">${items}</div>`, anchor);
}

async function _changeCategorySelected(catName, move) {
    const entries = [..._selected.entries()];
    _hideCtxMenu();

    // Each torrent is addressed on the engine that currently holds it. The
    // endpoint works out the rest: relabel, hand over to the other engine, or
    // move the payload, depending on what the target category asks for.
    const post = (hash, mode, allowBreakingHardlinks) =>
        fetch(`/api/${mode}/torrents/${hash}/category`, {
            method: "POST",
            headers: {
                "X-Api-Key": API_KEY,
                "Content-Type": "application/json",
            },
            body: JSON.stringify({
                category: catName,
                move_files: !!move,
                allow_breaking_hardlinks: !!allowBreakingHardlinks,
            }),
        });

    let okCount = 0;
    let movingCount = 0;
    // Only a 200 means the category is already what the row should show. A 202
    // is a job that has not moved a byte yet and can still fail, so painting
    // the row now would state a result that does not exist.
    const relabelled = [];
    const errors = [];
    const needConsent = [];

    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        const mode = _selMode(sel);
        try {
            const r = await post(hash, mode, false);
            let j = null;
            try { j = await r.json(); } catch (_) { /* empty body */ }
            if (r.status === 409 && j && j.reason === "hardlinks") {
                // Not a failure: a question. Collected and asked once below,
                // rather than one prompt per torrent.
                needConsent.push({ hash, mode, files: j.hardlinked_files || 0, bytes: j.hardlinked_bytes || 0 });
                continue;
            }
            if (!r.ok) {
                errors.push(`${hash.slice(0, 8)}: ${(j && j.error) || ("HTTP " + r.status)}`);
                continue;
            }
            if (r.status === 202) { movingCount++; } else { okCount++; relabelled.push(hash); }
        } catch (err) {
            errors.push(`${hash.slice(0, 8)}: ${err.message}`);
        }
    }

    if (needConsent.length > 0) {
        const files = needConsent.reduce((n, x) => n + x.files, 0);
        const bytes = needConsent.reduce((n, x) => n + x.bytes, 0);
        const question = tp(needConsent.length,
            "{n} torrent cannot move without breaking hardlinks.",
            "{n} torrents cannot move without breaking hardlinks.", { n: needConsent.length })
            + "\n\n"
            + t("{files} file(s), {size}, are hardlinked elsewhere (usually the Sonarr or Radarr library). The target is on another filesystem, so copying them leaves a second full copy on disk.",
                { files: files, size: formatBytes(bytes) })
            + "\n\n" + t("Move them anyway?");
        if (await hydraConfirm(question)) {
            for (const { hash, mode } of needConsent) {
                try {
                    const r = await post(hash, mode, true);
                    if (!r.ok) {
                        let j = null;
                        try { j = await r.json(); } catch (_) { /* empty body */ }
                        errors.push(`${hash.slice(0, 8)}: ${(j && j.error) || ("HTTP " + r.status)}`);
                    } else if (r.status === 202) {
                        movingCount++;
                    } else {
                        okCount++;
                        relabelled.push(hash);
                    }
                } catch (err) {
                    errors.push(`${hash.slice(0, 8)}: ${err.message}`);
                }
            }
        }
    }

    if (errors.length > 0) {
        hydraNotify(t("Category changed to \"{cat}\": {ok} OK, {failed} failure(s).", { cat: catName, ok: okCount + movingCount, failed: errors.length }) + "\n\n" + errors.join("\n"));
    } else if (movingCount > 0) {
        // A move runs in the background and can take hours, so say so rather
        // than leaving the row looking like nothing happened.
        hydraNotify(tp(movingCount,
            "Moving {n} torrent to \"{cat}\" in the background. It keeps seeding while its data is copied; follow it in Jobs.",
            "Moving {n} torrents to \"{cat}\" in the background. They keep seeding while their data is copied; follow them in Jobs.",
            { n: movingCount, cat: catName }));
    }

    // Optimistic, but only for the rows that really were relabelled (HTTP 200).
    // A row whose payload is being moved in the background keeps its current
    // category until the job has actually finished: the move can fail hours
    // later, and a row repainted at click time would have been lying the whole
    // time. Race rows are rendered from freshly fetched data and need no nudge.
    if (Array.isArray(_hoardAllTorrents)) {
        for (const hash of relabelled) {
            const t = _hoardAllTorrents.find(x => x.info_hash === hash);
            if (t) t.category = catName;
        }
    }
    try { if (typeof _scheduleHoardRender === "function") _scheduleHoardRender(); } catch (_) {}
}

document.addEventListener("click", e => {
    if (!e.target.closest("#ctx-menu") && !e.target.closest("#ctx-submenu")) _hideCtxMenu();
});

document.addEventListener("keydown", e => {
    if (e.key === "Escape") {
        _hideCtxMenu();
        _selected.clear();
        _updateRowHighlights();
        closeDetail();
        closeHoardDetail();
    }
});

document.addEventListener("click", e => {
    if (e.target.closest(".t-row") ||
        e.target.closest("#torrent-detail") ||
        e.target.closest("#hoard-detail-panel") ||
        e.target.closest("#ctx-menu") ||
        e.target.closest("#ctx-submenu") ||
        e.target.closest(".modal-overlay")) return;
    _hideCtxMenu();
    _selected.clear();
    _updateRowHighlights();
    closeDetail();
    closeHoardDetail();
});

async function _reannounceSelected() {
    _hideCtxMenu();
    const entries = [..._selected.entries()];
    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        try {
            if (!_isLocalAgent(_selAgent(sel))) {
                await _agentAction(_selAgent(sel), _selMode(sel), "reannounce", hash);
                continue;
            }
            await fetch(`/api/torrents/${hash}/reannounce`, {
                method: "POST",
                headers: { "X-Api-Key": API_KEY },
            });
        } catch (err) {
            console.error("Failed to reannounce", hash, err);
        }
    }
}

// Recheck = hash-check the torrent data on disk (engine verify), resuming
// from the verified state. Hoard-only: there is no race verify endpoint,
// and the ctx item is hidden unless the selection has hoard rows. The hoard
// verify route already forwards to the owning agent for remote torrents.
// Whether the user paused this torrent: true, false, or null when we cannot
// tell (only the hoard list is held in memory). The engine is authoritative —
// this just decides which menu entries are worth offering.
// What the row should say. "Stopped" is the user's own decision; "Queued" is a
// scheduler holding the torrent back, which is normal and temporary. Keeping
// them apart is the whole point of the intent flag, same split qBittorrent 5
// makes, so the words already mean what people expect.
function displayState(t) {
    // Broken wins over every other label: a torrent whose data cannot be read
    // is not merely stopped, and that is the one thing worth seeing at a glance.
    if (t.state === "error") {
        return {
            label: window.t("error"),
            cls: "state-error",
            title: t.torrent_error_msg || window.t("Its data could not be read"),
        };
    }
    if (t.user_paused || t.state === "stopped") {
        return { label: window.t("stopped"), cls: "state-paused", title: window.t("Stopped by you, survives a restart") };
    }
    if (t.state === "queued") {
        return { label: window.t("queued"), cls: "state-queued", title: window.t("Waiting for a slot") };
    }
    return {
        label: window.t(t.state),
        cls: t.state === "seeding" ? "state-active" : "state-checking",
        title: window.t(t.state),
    };
}

function _isUserPaused(hash) {
    const t = _hoardAllTorrents.find(x => x.info_hash === hash);
    // The state is the fresher signal: the list snapshot carries it on every
    // refresh, while user_paused only rides along with a full list fetch.
    return t ? (!!t.user_paused || t.state === "stopped") : null;
}

// Reflect a stop/start in the browser's copy right away, so the context menu
// offers the opposite action on the next right-click instead of waiting for
// the 30s list refresh to catch up.
function _markLocallyStopped(hashes, stopped) {
    const set = new Set(hashes);
    for (const t of _hoardAllTorrents) {
        if (!set.has(t.info_hash)) continue;
        t.user_paused = stopped;
        t.state = stopped ? "stopped" : "queued";
    }
    renderHoardTable();
    _renderHoardCounts();
}

// Stop or start the selection. This writes the user's intent: it outlives a
// restart and no scheduler will undo it.
//
// Past a threshold the selection stops travelling as hashes and travels as the
// filter that produced it -- the daemon already holds the list, so shipping
// 89k hashes back to it would be a multi-megabyte way of saying "the ones I am
// looking at". The daemon answers with the count it matched, and we compare it
// against what the table showed: the filter exists on both sides, so the thing
// worth catching is the two drifting apart.
const BULK_FILTER_THRESHOLD = 500;

async function _pauseSelected(paused) {
    _hideCtxMenu();
    const action = paused ? "stop" : "start";

    // Big selection that IS the filtered set (with at most a few rows
    // deselected): send the filter, not the hashes.
    if (_selected.size > BULK_FILTER_THRESHOLD && _hoardFiltered.length) {
        const excluded = _hoardFiltered
            .filter(t => !_selected.has(_selKeyOf(t.info_hash, t.agent)))
            .map(t => t.info_hash);
        // Only worth it while the selection really is "the filter minus a few".
        if (excluded.length * 4 < _selected.size) {
            if (!await hydraConfirm(action === "stop" ? t("Stop {n} torrents?", { n: _selected.size }) : t("Start {n} torrents?", { n: _selected.size }))) return;
            try {
                const r = await fetch("/api/hoard/torrents/bulk", {
                    method: "POST",
                    headers: { "X-Api-Key": API_KEY, "Content-Type": "application/json" },
                    body: JSON.stringify({ action, filter: _currentHoardFilter(), exclude: excluded }),
                });
                const j = await r.json();
                if (j && typeof j.matched === "number" && j.matched !== _selected.size) {
                    console.warn(`bulk ${action}: server matched ${j.matched}, UI had ${_selected.size}`);
                    hydraNotify(t("Heads up: the server matched {matched} torrents, the table showed {shown}. Applied to {applied}.", { matched: j.matched, shown: _selected.size, applied: j.applied }));
                }
                _markLocallyStopped(
                    _hoardFiltered.map(t => t.info_hash).filter(h => !excluded.includes(h)),
                    paused);
            } catch (err) {
                console.error(`Failed to ${action} in bulk`, err);
            }
            updateHoardStats();
            return;
        }
    }

    const byEngine = { hoard: [], race: [] };
    const remote = [];
    for (const sel of _selected.values()) {
        const hash = _selHash(sel);
        const mode = _selMode(sel);
        const agent = _selAgent(sel);
        if (!_isLocalAgent(agent)) { remote.push({ hash, mode, agent }); continue; }
        (byEngine[mode] || byEngine.hoard).push(hash);
    }
    // The bulk endpoint below is this node's own; an agent's copy would
    // otherwise be silently skipped, or worse, applied to the local twin.
    for (const r of remote) {
        try {
            await _agentAction(r.agent, r.mode, paused ? "pause" : "resume", r.hash);
        } catch (err) {
            console.error("Failed to " + action + " on " + r.agent, r.hash, err);
        }
    }
    for (const engine of ["hoard", "race"]) {
        const hashes = byEngine[engine];
        if (!hashes.length) continue;
        try {
            await fetch(`/api/${engine}/pause`, {
                method: "POST",
                headers: { "X-Api-Key": API_KEY, "Content-Type": "application/json" },
                body: JSON.stringify({ hashes, paused }),
            });
        } catch (err) {
            console.error(`Failed to ${action} ${engine}`, err);
        }
    }
    _markLocallyStopped(byEngine.hoard, paused);
    updateHoardStats();
}

// The filter the hoard table is currently applying, in the shape the daemon
// expects. Kept next to renderHoardTable's filtering so the two stay in step.
function _currentHoardFilter() {
    return {
        search: (document.getElementById("hoard-search")?.value || ""),
        category: _hoardCatFilter || "",
        tracker: _hoardTrackerFilter || "",
        tag: _hoardTagFilter || "",
        state: _hoardStateFilter || "",
    };
}

async function _recheckSelected() {
    _hideCtxMenu();
    const entries = [..._selected.entries()];
    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        const mode = _selMode(sel);
        if (mode !== "hoard") continue;
        const agent = _selAgent(sel);
        try {
            if (!_isLocalAgent(agent)) {
                await _agentAction(agent, mode, "verify", hash);
                continue;
            }
            await fetch(`/api/hoard/torrents/${hash}/verify`, {
                method: "POST",
                headers: { "X-Api-Key": API_KEY },
            });
        } catch (err) {
            console.error("Failed to recheck", hash, err);
        }
    }
    updateHoardStats();
}

async function _removeSelected(deleteFiles) {
    _hideCtxMenu();
    const entries = [..._selected.entries()];
    _selected.clear();
    _updateRowHighlights();
    for (const [, sel] of entries) {
        const hash = _selHash(sel);
        try {
            await fetch(`/api/torrents/${hash}?delete_files=${deleteFiles}&agent=${encodeURIComponent(_selAgent(sel))}`, {
                method: "DELETE",
                headers: { "X-Api-Key": API_KEY },
            });
            if (selectedTorrent === hash) closeDetail();
            if (selectedHoardTorrent === hash) closeHoardDetail();
        } catch (err) {
            console.error("Failed to remove", hash, err);
        }
    }
    updateRaceTorrents();
    updateHoardStats();
    updateOverview();
}

async function refreshHoardDetail() {
    if (!selectedHoardTorrent) return;
    try {
        const q = selectedHoardTorrentAgent && !_isLocalAgent(selectedHoardTorrentAgent)
            ? `?agent=${encodeURIComponent(selectedHoardTorrentAgent)}` : "";
        const d = await api(`/api/hoard/torrents/${selectedHoardTorrent}${q}`);

        document.getElementById("h-detail-name").textContent = incoName(d);
        document.getElementById("h-detail-state").textContent = d.state;
        // Say why it broke, and what to do about it. Nothing clears this on its
        // own: the serve path is suspended, so no read will ever retry.
        const hErr = document.getElementById("h-detail-error");
        if (hErr) {
            if (d.state === "error" || d.torrent_error) {
                hErr.style.display = "";
                hErr.innerHTML = `<strong>${esc(window.t("This torrent cannot read its data"))}</strong>`
                    + `<div class="detail-error-msg">${esc(d.torrent_error_msg || window.t("unknown error"))}</div>`
                    + `<div class="detail-error-hint">${esc(window.t("Restore the files, then recheck to bring it back"))}</div>`;
            } else {
                hErr.style.display = "none";
                hErr.innerHTML = "";
            }
        }
        document.getElementById("h-detail-progress").textContent = (d.progress * 100).toFixed(1) + "%";
        document.getElementById("h-detail-size").textContent = formatBytes(d.total_size);
        document.getElementById("h-detail-downloaded").textContent = formatBytes(d.total_download);
        document.getElementById("h-detail-uploaded").textContent = formatBytes(d.total_upload);
        document.getElementById("h-detail-ratio").textContent = displayRatio(d).toFixed(4);
        document.getElementById("h-detail-path").textContent = incoPath(d.save_path);
        document.getElementById("h-detail-hash").textContent = d.info_hash;
        renderPieceMap(d.pieces_have, d.pieces_avail, "h-detail-pieces-canvas", "h-detail-pieces-info", "h-detail-pieces-card");
        document.getElementById("h-detail-dl-speed").textContent = formatSpeed(d.download_rate);
        document.getElementById("h-detail-ul-speed").textContent = formatSpeed(d.upload_rate);
        document.getElementById("h-detail-avg-ul").textContent = formatSpeed(d.avg_upload_rate || 0);
        document.getElementById("h-detail-peers-count").textContent = d.num_peers;
        document.getElementById("h-detail-seeds-count").textContent = d.num_seeds;
        document.getElementById("h-detail-pieces").textContent = d.num_pieces;
        document.getElementById("h-detail-piece-size").textContent = formatBytes(d.piece_length);
        document.getElementById("h-detail-active-time").textContent = formatDuration(d.active_time);
        document.getElementById("h-detail-seeding-time").textContent = formatDuration(d.seeding_time);

        const ptbody = document.getElementById("h-detail-peers-tbody");
        if (d.peers && d.peers.length > 0) {
            ptbody.innerHTML = d.peers.map(p => {
                const flags = (p.flags || []).map(f =>
                    `<span class="peer-flag ${f}">${f}</span>`
                ).join("");
                return `<tr>
                    <td>${incoIP(p.ip)}:${p.port}</td>
                    <td>${p.client || "-"}</td>
                    <td>${(p.progress * 100).toFixed(0)}%</td>
                    <td>${formatSpeed(p.down_speed)}</td>
                    <td>${formatSpeed(p.up_speed)}</td>
                    <td>${formatBytes(p.total_download)}</td>
                    <td>${formatBytes(p.total_upload)}</td>
                    <td>${flags || "-"}</td>
                </tr>`;
            }).join("");
        } else {
            ptbody.innerHTML = '<tr><td colspan="8" class="empty">No peers connected</td></tr>';
        }

        // Same freeze as the race panel: never redraw a list being edited.
        if (trackerEditorIsOpen()) return;
        const ttbody = document.getElementById("h-detail-trackers-tbody");
        if (d.trackers && d.trackers.length > 0) {
            ttbody.innerHTML = d.trackers.map(t => {
                let domain = t.url;
                try { domain = new URL(t.url).hostname; } catch (_) {}
                const ep = t.endpoints && t.endpoints[0];
                const hasErr = ep && ep.last_error && ep.last_error !== "Success";
                const msg = ep ? (ep.message || ep.last_error || "") : "";
                const nextAnn = ep && ep.next_announce !== undefined ? ep.next_announce : -1;
                const lastAnn = ep && ep.last_announce !== undefined ? ep.last_announce : -1;
                const seeds = ep ? ep.scrape_complete : -1;
                const leechers = ep ? ep.scrape_incomplete : -1;
                const statusHtml = hasErr
                    ? `<span class="tracker-err" title="${msg}">${msg.substring(0, 60) || "error"}</span>`
                    : `<span class="tracker-ok">${msg || "OK"}</span>`;
                const nextStr = nextAnn > 0 ? `${Math.floor(nextAnn/60)}m${nextAnn%60}s` : nextAnn === 0 ? "now" : "-";
                const scrapeStr = seeds >= 0 ? `${seeds}s/${leechers}l` : "";
                return `<tr><td class="mono" title="${t.url}">${domain}</td><td>${statusHtml}</td><td class="mono">${formatAgo(lastAnn)}</td><td class="mono">${nextStr}</td><td class="mono">${scrapeStr}</td></tr>`;
            }).join("");
        } else {
            ttbody.innerHTML = '<tr><td colspan="5" class="empty">No trackers</td></tr>';
        }
    } catch (e) {
        console.error("Failed to refresh hoard detail:", e);
    }
}

// ─── Seedbox badge ──────────────────────────────────────

function buildSeedboxBadge(p) {
    const speedOk = p.avg_speed > 10_000_000;
    const reliabilityOk = p.reliability > 0.8;
    const sessionsOk = p.num_sessions > 10;

    const checks = [
        (speedOk ? "" : "✗ ") + t("Avg speed > 10 MB/s ({v})", { v: formatSpeed(p.avg_speed) }),
        (reliabilityOk ? "" : "✗ ") + t("Reliability > 80% ({v}%)", { v: (p.reliability * 100).toFixed(0) }),
        (sessionsOk ? "" : "✗ ") + t("Sessions > 10 ({v})", { v: p.num_sessions }),
    ].join("\n");

    if (p.is_seedbox) {
        return `<span class="badge-seedbox" title="${checks}">SEEDBOX ℹ</span>`;
    }
    const failed = [!speedOk && "speed", !reliabilityOk && "reliability", !sessionsOk && "sessions"].filter(Boolean).join(", ");
    return `<span class="badge-no" title="${checks}">✗ ${failed}</span>`;
}

// ─── Remove torrent ─────────────────────────────────────

function removeTorrent(infoHash) {
    const modal = document.getElementById("remove-modal");
    modal.dataset.hash = infoHash;
    modal.style.display = "flex";
}

function closeRemoveModal() {
    document.getElementById("remove-modal").style.display = "none";
}

async function confirmRemove(deleteFiles) {
    const modal = document.getElementById("remove-modal");
    const infoHash = modal.dataset.hash;
    modal.style.display = "none";

    try {
        await fetch(`/api/torrents/${infoHash}?delete_files=${deleteFiles}`, {
            method: "DELETE",
            headers: { "X-Api-Key": API_KEY },
        });
        if (selectedTorrent === infoHash) closeDetail();
        updateRaceTorrents();
        updateOverview();
    } catch (e) {
        hydraNotify(t("Failed to remove: {msg}", { msg: e.message }));
    }
}

// ─── Add torrent ────────────────────────────────────────

// Bencode decoder. Strings stay raw (Uint8Array) so we can decode them as
// UTF-8 only where they are meant to be text; `pieces` is binary.
function _bdecode(buf) {
    let i = 0;
    const dec = new TextDecoder("utf-8");
    function readInt(stop) {
        const j = buf.indexOf(stop, i);
        if (j < 0) throw new Error("truncated");
        const n = parseInt(dec.decode(buf.subarray(i, j)), 10);
        i = j + 1;
        return n;
    }
    function parse() {
        const c = buf[i];
        if (c === undefined) throw new Error("truncated");
        if (c === 0x69) { i++; return readInt(0x65); }            // i<n>e
        if (c === 0x6c) {                                          // l...e
            i++;
            const a = [];
            while (buf[i] !== 0x65) a.push(parse());
            i++;
            return a;
        }
        if (c === 0x64) {                                          // d...e
            i++;
            const o = {};
            while (buf[i] !== 0x65) {
                const k = parse();
                o[k instanceof Uint8Array ? dec.decode(k) : String(k)] = parse();
            }
            i++;
            return o;
        }
        const len = readInt(0x3a);                                 // <len>:<bytes>
        if (!(len >= 0)) throw new Error("bad string length");
        const out = buf.subarray(i, i + len);
        i += len;
        return out;
    }
    return parse();
}

function _btext(v) {
    if (v instanceof Uint8Array) { try { return new TextDecoder("utf-8").decode(v); } catch (_) { return ""; } }
    return v == null ? "" : String(v);
}

// Pull the human-readable summary out of a raw .torrent buffer.
function _torrentSummary(bytes) {
    const t = _bdecode(bytes);
    const info = t && t.info;
    if (!info) throw new Error(window.t("no info dictionary"));
    const name = _btext(info["name.utf-8"] || info.name);
    let files;
    if (Array.isArray(info.files)) {
        files = info.files.map(f => ({
            path: (f["path.utf-8"] || f.path || []).map(_btext).join("/"),
            length: f.length || 0,
        }));
    } else {
        files = [{ path: name, length: info.length || 0 }];
    }
    const trackers = [];
    const addTr = u => { const v = _btext(u); if (v && trackers.indexOf(v) < 0) trackers.push(v); };
    if (t.announce) addTr(t.announce);
    if (Array.isArray(t["announce-list"])) t["announce-list"].forEach(tier => (tier || []).forEach(addTr));
    return {
        name,
        files,
        total: files.reduce((a, f) => a + f.length, 0),
        trackers,
        pieceLength: info["piece length"] || 0,
        pieceCount: info.pieces instanceof Uint8Array ? Math.floor(info.pieces.length / 20) : 0,
        isPrivate: info.private === 1,
        comment: _btext(t.comment),
        createdBy: _btext(t["created by"]),
    };
}

const _PREVIEW_MAX_ROWS = 400;

function _previewCardHTML(fileName, sum) {
    const rows = sum.files.slice(0, _PREVIEW_MAX_ROWS).map(f =>
        `<div class="tp-file"><span class="tp-file-path">${esc(f.path)}</span><span class="tp-file-size">${formatBytes(f.length)}</span></div>`
    ).join("");
    const hidden = sum.files.length - _PREVIEW_MAX_ROWS;
    const more = hidden > 0 ? `<div class="tp-more">${tp(hidden, "and {n} more file", "and {n} more files")}</div>` : "";
    const meta = [
        tp(sum.files.length, "{n} file", "{n} files"),
        formatBytes(sum.total),
        sum.pieceLength ? t("{size} pieces ({n})", { size: formatBytes(sum.pieceLength), n: sum.pieceCount }) : "",
        sum.isPrivate ? t("private") : "",
    ].filter(Boolean).join(" · ");
    const trackers = sum.trackers.length
        ? `<div class="tp-trackers">${sum.trackers.slice(0, 8).map(u => esc(u)).join("<br>")}` +
          (sum.trackers.length > 8 ? "<br>" + t("and {n} more", { n: sum.trackers.length - 8 }) : "") + `</div>`
        : `<div class="tp-trackers">${t("no tracker (DHT only)")}</div>`;
    return `<details class="tp-card" open>
        <summary><span class="tp-name">${esc(sum.name || fileName)}</span><span class="tp-meta">${meta}</span></summary>
        <div class="tp-files">${rows}${more}</div>
        ${trackers}
    </details>`;
}

function _previewErrorHTML(fileName, msg) {
    return `<div class="tp-card tp-card-error"><span class="tp-name">${esc(fileName)}</span>` +
        `<span class="tp-meta">${t("not a readable .torrent, {err}", { err: esc(msg) })}</span></div>`;
}

function _previewEnabled() {
    return localStorage.getItem("hydra_add_preview") !== "0";
}

async function renderTorrentPreview() {
    const box = document.getElementById("torrent-preview");
    const input = document.getElementById("torrent-upload");
    if (!box || !input) return;
    const col = document.querySelector(".add-preview-col");
    const files = Array.from(input.files || []);
    if (!_previewEnabled()) {
        if (col) col.style.display = "none";
        box.innerHTML = "";
        return;
    }
    if (col) col.style.display = "";
    if (files.length === 0) {
        box.innerHTML = `<div class="tp-empty">Pick one or more .torrent files above to see what is inside them.` +
            ` They are read in your browser and nothing is sent until you press Add Torrent.</div>`;
        return;
    }
    box.innerHTML = `<div class="tp-head">Reading ${files.length} file${files.length > 1 ? "s" : ""}…</div>`;
    const cards = [];
    for (const f of files) {
        try {
            const buf = new Uint8Array(await f.arrayBuffer());
            cards.push(_previewCardHTML(f.name, _torrentSummary(buf)));
        } catch (err) {
            cards.push(_previewErrorHTML(f.name, err.message || String(err)));
        }
    }
    // The selection may have changed while we were reading.
    if (Array.from(input.files || []).length !== files.length) return;
    const head = files.length > 1 ? `<div class="tp-head">${files.length} torrents selected</div>` : "";
    box.innerHTML = head + cards.join("");
}

(function initTorrentPreview() {
    const input = document.getElementById("torrent-upload");
    const toggle = document.getElementById("torrent-preview-enabled");
    if (!input || !toggle) return;
    toggle.checked = _previewEnabled();
    toggle.addEventListener("change", () => {
        localStorage.setItem("hydra_add_preview", toggle.checked ? "1" : "0");
        renderTorrentPreview();
    });
    input.addEventListener("change", renderTorrentPreview);
    renderTorrentPreview();
})();


// The two per-add overrides of the Add form. They start on the daemon's own
// defaults so the form states what is going to happen instead of guessing it.
(async function initAddOptions() {
    const sub = document.getElementById("add-create-subfolder");
    const skip = document.getElementById("add-skip-recheck");
    if (!sub || !skip) return;
    try {
        const d = await api("/api/torrents/add-defaults");
        sub.checked = !!d.create_subfolder;
        skip.checked = !!d.skip_recheck;
        sub.dataset.ready = "1";
    } catch (_) {
        // Defaults unknown: send nothing rather than a guess (see below).
    }
})();

// A checkbox only overrides the daemon once the form knows what the daemon
// default is. Before that, sending its unchecked state would silently turn the
// setting off for that add.
function _addOverrides() {
    const sub = document.getElementById("add-create-subfolder");
    const skip = document.getElementById("add-skip-recheck");
    return {
        create_subfolder: (sub && sub.dataset.ready) ? sub.checked : undefined,
        skip_recheck: !!(skip && skip.checked),
    };
}

let _addMsgTimer = null;
document.getElementById("add-torrent-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const resultEl = document.getElementById("add-result");
    const btn = document.getElementById("add-btn");
    const _modeBtn = document.querySelector(".mode-btn.active");
    const mode = _modeBtn ? _modeBtn.dataset.mode : "race";
    const fileInput = document.getElementById("torrent-upload");

    btn.disabled = true;
    if (!btn.dataset.label) btn.dataset.label = btn.textContent;
    btn.textContent = t("Adding…");
    resultEl.className = "result-msg";
    resultEl.style.display = "none";

    try {
        let result;

        const category = document.getElementById("torrent-category").value;

        if (fileInput.files.length > 0) {
            const files = Array.from(fileInput.files);
            const savePath = document.getElementById("save-path").value;

            if (files.length === 1) {
                // Upload simple
                const formData = new FormData();
                formData.append("file", files[0]);
                formData.append("mode", mode);
                formData.append("save_path", savePath);
                if (category) formData.append("category", category);
                const ov = _addOverrides();
                if (ov.create_subfolder !== undefined) formData.append("create_subfolder", String(ov.create_subfolder));
                if (ov.skip_recheck) formData.append("skip_recheck", "true");
                const res = await fetch("/api/torrents/upload", {
                    method: "POST",
                    headers: { "X-Api-Key": API_KEY },
                    body: formData,
                });
                if (!res.ok) throw new Error(await res.text());
                result = await res.json();
            } else {
                // Bulk upload
                let ok = 0, fail = 0, errors = [];
                for (let i = 0; i < files.length; i++) {
                    resultEl.textContent = t("Upload {i}/{total}, {name}", { i: i + 1, total: files.length, name: files[i].name });
                    resultEl.className = "result-msg";
                    resultEl.style.display = "block";
                    try {
                        const formData = new FormData();
                        formData.append("file", files[i]);
                        formData.append("mode", mode);
                        formData.append("save_path", savePath);
                        if (category) formData.append("category", category);
                        const ov = _addOverrides();
                        if (ov.create_subfolder !== undefined) formData.append("create_subfolder", String(ov.create_subfolder));
                        if (ov.skip_recheck) formData.append("skip_recheck", "true");
                        const res = await fetch("/api/torrents/upload", {
                            method: "POST",
                            headers: { "X-Api-Key": API_KEY },
                            body: formData,
                        });
                        if (!res.ok) { errors.push(`${files[i].name}: ${await res.text()}`); fail++; }
                        else ok++;
                    } catch (e) {
                        errors.push(`${files[i].name}: ${e.message}`); fail++;
                    }
                }
                if (fail === 0) {
                    resultEl.textContent = tp(ok, "{n} torrent added", "{n} torrents added");
                    resultEl.className = "result-msg success";
                } else {
                    resultEl.innerHTML = t("{ok} OK, {failed} failed:", { ok: ok, failed: fail }) + "<br>" +
                        errors.map(e => `<small>${e}</small>`).join("<br>");
                    resultEl.className = "result-msg " + (ok > 0 ? "" : "error");
                }
                resultEl.style.display = "block";
                clearTimeout(_addMsgTimer);
                if (fail === 0) _addMsgTimer = setTimeout(() => { resultEl.style.display = "none"; }, 6000);
                fileInput.value = "";
                btn.textContent = btn.dataset.label || t("Add Torrent");
                btn.disabled = false;
                updateOverview(); updateRaceTorrents(); updateHoardStats();
                return;
            }
        } else {
            // Path or magnet mode
            const ov = _addOverrides();
            const body = {
                mode: mode,
                save_path: document.getElementById("save-path").value,
                category: category || undefined,
                create_subfolder: ov.create_subfolder,
                skip_recheck: ov.skip_recheck,
            };

            const torrentPath = document.getElementById("torrent-path").value.trim();
            const magnetUri = document.getElementById("magnet-uri").value.trim();

            if (torrentPath) {
                body.torrent_path = torrentPath;
            } else if (magnetUri && mode === "race") {
                body.magnet_uri = magnetUri;
            } else {
                throw new Error(t("Provide a torrent path, magnet URI, or upload a file"));
            }

            result = await api("/api/torrents", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(body),
            });
        }

        const _fname = (fileInput.files && fileInput.files[0])
            ? fileInput.files[0].name.replace(/\.torrent$/i, "")
            : ((document.getElementById("torrent-path").value.split("/").pop() || "").replace(/\.torrent$/i, "") || (result.info_hash || "").slice(0, 16));
        const _cat = document.getElementById("torrent-category").value;
        resultEl.innerHTML = `Added <b>${esc(_fname)}</b> \u2192 <b>${esc(result.mode)}</b>${_cat ? " \u00b7 " + esc(_cat) : ""}`;
        resultEl.className = "result-msg success";
        resultEl.style.display = "";
        clearTimeout(_addMsgTimer);
        _addMsgTimer = setTimeout(() => { resultEl.style.display = "none"; }, 6000);
        btn.textContent = "Added";
        setTimeout(() => { btn.textContent = btn.dataset.label || t("Add Torrent"); }, 1500);

        // Reset form
        document.getElementById("torrent-path").value = "";
        document.getElementById("magnet-uri").value = "";
        fileInput.value = "";

        // Refresh lists
        updateOverview();
        updateRaceTorrents();
        updateHoardStats();
    } catch (err) {
        resultEl.textContent = t("Error: {msg}", { msg: err.message });
        resultEl.className = "result-msg error";
        resultEl.style.display = "";
        btn.textContent = btn.dataset.label || t("Add Torrent");
    } finally {
        btn.disabled = false;
    }
});

// ─── Categories ─────────────────────────────────────────

let _editingCategory = null;
// Markup of the rows currently in the categories table. The table is rebuilt on
// a timer, so without this every refresh replaced identical HTML and the screen
// flickered for nothing.
let _lastCategoryRows = null;
// What each orphaned label's torrents are actually doing, in render order, so
// adoption can start from that rather than from the form's defaults.
let _orphanInfo = [];

async function updateCategories() {
    try {
        // One paint, not two. Categories and the labels wearing no category are
        // separate calls, and painting the first before the second arrived made
        // the table visibly rebuild itself: rows appeared, then the whole body
        // was replaced a round trip later. Fetch both, render once -- and only
        // touch the DOM when the markup actually changed, because this runs on
        // a timer against a screen that is static almost all of the time.
        const [cats, orphans] = await Promise.all([
            api("/api/categories"),
            api("/api/categories/orphans").catch(e => {
                console.error("load orphaned categories", e);
                return [];
            }),
        ]);
        const tbody = document.getElementById("categories-tbody");

        const catRows = (cats || []).map(cat => `<tr>
                <td><strong>${esc(incoCat(cat.name))}</strong></td>
                <td><span class="mode-tag mode-${cat.mode}">${cat.mode}</span></td>
                <td class="mono" style="font-size:12px">${esc(incoPath(cat.save_path))}</td>
                <td>${((cat.placement && cat.placement.length) ? cat.placement : ["local"]).map(esc).join(", ")}</td>
                <td>${esc(cat.strategy || "all")}</td>
                <td>${cat.graduate_to ? '\u2192 ' + esc(incoCat(cat.graduate_to)) : '<span style="color:var(--text-muted)">\u2014</span>'}</td>
                <td>
                    <button class="btn-small" onclick="editCategory('${cat.name}')">Edit</button>
                    <button class="btn-small btn-danger" onclick="deleteCategory('${cat.name}')">Delete</button>
                </td>
            </tr>`).join("");

        // Labels worn by torrents but matching no configured category: the
        // residue of deletions made before labels were cleared durably, and of
        // upgrades that lost the definitions while the torrents kept the names.
        // Shown here because this screen holds the only actions on them, and
        // left out of the dropdown below, since you cannot assign one that is
        // gone.
        // Indexed, not inlined: a save path may hold a quote, and passing it
        // through an onclick attribute would break the handler.
        _orphanInfo = orphans || [];
        const orphanRows = (orphans || []).map((o, i) => `<tr>
                    <td><strong>${esc(incoCat(o.name))}</strong> <span style="color:var(--text-muted);font-size:11px">orphaned</span></td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td class="mono" style="font-size:12px;color:var(--text-muted)">no longer configured, still on ${o.torrents} torrent${o.torrents > 1 ? "s" : ""}</td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td>
                        <button class="btn-small" onclick="adoptCategory(${i})">${t("Adopt")}</button>
                        <button class="btn-small btn-danger" onclick="deleteCategory('${o.name}')">Delete</button>
                    </td>
                </tr>`).join("");

        const html = (catRows + orphanRows) ||
            '<tr><td colspan="7" class="empty">No categories</td></tr>';
        if (html !== _lastCategoryRows) {
            tbody.innerHTML = html;
            _lastCategoryRows = html;
        }

        refreshCategoryOptions(cats);
    } catch (e) {
        console.error("Failed to update categories:", e);
    }
}

// The add form's category picker, split out of updateCategories() so it no
// longer waits on the categories tab being opened: that tab's rollups are too
// heavy to run on every load, and the dropdown sat empty until they did.
async function refreshCategoryOptions(cats) {
    const sel = document.getElementById("torrent-category");
    if (!sel) return;
    if (!cats) {
        try { cats = await api("/api/categories"); }
        catch (e) { console.error("load categories", e); return; }
    }
    const current = sel.value;
    sel.innerHTML = '<option value="">- none -</option>' +
        (cats || []).map(cat => `<option value="${cat.name}"${cat.name === current ? " selected" : ""}>${esc(incoCat(cat.name))}</option>`).join("");
}

async function showCategoryForm(name = null) {
    _editingCategory = name;
    document.getElementById("cat-name").value = name || "";
    document.getElementById("cat-mode").value = "race";
    document.getElementById("cat-save-path").value = "";
    document.getElementById("cat-strategy").value = "all";
    document.getElementById("cat-min-free").value = "";
    document.getElementById("cat-result").style.display = "none";
    document.getElementById("category-form").style.display = "block";

    let allCats = [];
    try { allCats = (await api("/api/categories")) || []; } catch (e) { console.error("load categories", e); }
    let cat = name ? (allCats.find(c => c.name === name) || null) : null;
    if (cat) {
        document.getElementById("cat-mode").value = cat.mode;
        document.getElementById("cat-save-path").value = cat.save_path;
        document.getElementById("cat-strategy").value = cat.strategy || "all";
        // Stored in bytes, shown in GiB: nobody types a reserve in bytes.
        document.getElementById("cat-min-free").value = cat.min_free_bytes
            ? Math.round(cat.min_free_bytes / (1024 * 1024 * 1024))
            : "";
    }
    _populateGraduateSelect(allCats, cat ? cat.graduate_to : "", name);
    _catModeChanged();
    await _renderCatPlacement(cat);
}
function _populateGraduateSelect(cats, current, selfName) {
    const sel = document.getElementById("cat-graduate-to");
    if (!sel) return;
    const hoard = (cats || []).filter(c => c.mode === "hoard" && c.save_path && c.name !== selfName);
    sel.innerHTML = '<option value="">\u2014 none (no graduation) \u2014</option>' +
        hoard.map(c => `<option value="${esc(c.name)}">${esc(c.name)}</option>`).join("");
    sel.value = current || "";
}
function _catModeChanged() {
    const race = document.getElementById("cat-mode").value === "race";
    const w = document.getElementById("cat-graduate-wrap");
    if (w) w.style.display = race ? "" : "none";
}

// _renderCatPlacement fills #cat-placement with one checkbox + save-path input
// per known agent, pre-selecting the editing category's placement/agents.
async function _renderCatPlacement(cat) {
    const box = document.getElementById("cat-placement");
    let agents = [];
    try { agents = await api("/api/agents"); } catch (e) { agents = [{ name: "local" }]; }
    if (!agents || !agents.length) agents = [{ name: "local" }];
    const placement = (cat && cat.placement) ? cat.placement : (cat ? [] : ["local"]);
    const perAgent = (cat && cat.agents) ? cat.agents : {};
    box.innerHTML = agents.map(a => {
        const checked = placement.includes(a.name) ? " checked" : "";
        const path = esc(perAgent[a.name] || "");
        const online = a.online === false ? ' <span class="sr-desc">(offline)</span>' : "";
        return `<div class="cat-agent-row">
            <label class="cat-agent-lbl"><input type="checkbox" class="cat-agent-cb" value="${esc(a.name)}"${checked}> ${esc(a.name)}</label>${online}
            <input type="text" class="cat-agent-path" data-agent="${esc(a.name)}" placeholder="save path on ${esc(a.name)} (blank = flat)" value="${path}" autocomplete="off">
        </div>`;
    }).join("");
}

function hideCategoryForm() {
    document.getElementById("category-form").style.display = "none";
    const dd = document.getElementById("fs-dropdown");
    if (dd) dd.style.display = "none";
    _fsBrowsedPath = null;
    _editingCategory = null;
}

// ─── Filesystem Dropdown ─────────────────────────────────

let _fsDirs = [];
let _fsBrowsedPath = null;
let _fsActiveInput = null;

function _parseFsPath(val) {
    if (!val || val === "/") return { parent: "/", fragment: "" };
    if (val.endsWith("/")) return { parent: val.replace(/\/$/, "") || "/", fragment: "" };
    const i = val.lastIndexOf("/");
    return { parent: i === 0 ? "/" : val.substring(0, i), fragment: val.substring(i + 1) };
}

function _positionFsDropdown() {
    if (!_fsActiveInput) return;
    const dropdown = document.getElementById("fs-dropdown");
    const rect = _fsActiveInput.getBoundingClientRect();
    dropdown.style.top = rect.bottom + "px";
    dropdown.style.left = rect.left + "px";
    dropdown.style.width = rect.width + "px";
}

function _renderFsDropdown(browsedPath, dirs, fragment) {
    _fsBrowsedPath = browsedPath;
    _fsDirs = dirs;
    const dropdown = document.getElementById("fs-dropdown");

    // Breadcrumbs
    const parts = browsedPath === "/" ? [] : browsedPath.split("/").filter(Boolean);
    let crumbs = '<div class="fs-dropdown-crumbs"><span class="fs-crumb" data-path="/">/</span>';
    let acc = "";
    for (const p of parts) {
        acc += "/" + p;
        crumbs += ` <span class="fs-crumb" data-path="${acc.replace(/"/g, "&quot;")}">${p}</span> /`;
    }
    crumbs += "</div>";

    const filtered = fragment
        ? dirs.filter(d => d.toLowerCase().startsWith(fragment.toLowerCase()))
        : dirs;

    const items = filtered.length === 0
        ? '<div class="fs-dir-item fs-empty">- no folders -</div>'
        : filtered.map(d => {
            const full = browsedPath === "/" ? "/" + d : browsedPath + "/" + d;
            return `<div class="fs-dir-item" data-path="${full.replace(/"/g, "&quot;")}">📁 ${d}</div>`;
        }).join("");

    dropdown.innerHTML = crumbs + items;
    _positionFsDropdown();
    dropdown.style.display = "block";

    dropdown.querySelectorAll(".fs-crumb").forEach(el => {
        el.addEventListener("mousedown", e => {
            e.preventDefault();
            _fsBrowsedPath = null;
            _fsActiveInput.value = el.dataset.path + "/";
            _openFsDropdown();
        });
    });
    dropdown.querySelectorAll(".fs-dir-item[data-path]").forEach(el => {
        el.addEventListener("mousedown", e => {
            e.preventDefault();
            const path = el.dataset.path;
            _fsActiveInput.value = path + "/";
            _fsBrowsedPath = null;
            _openFsDropdown();
        });
    });
}

let _fsBlurTimer = null;

async function _openFsDropdown() {
    if (!_fsActiveInput) return;
    const val = _fsActiveInput.value.trim();
    const { parent, fragment } = _parseFsPath(val || "/");
    const dropdown = document.getElementById("fs-dropdown");

    if (_fsBrowsedPath === parent) {
        _renderFsDropdown(_fsBrowsedPath, _fsDirs, fragment);
        return;
    }

    _positionFsDropdown();
    dropdown.innerHTML = '<div class="fs-dir-item fs-empty">Loading…</div>';
    dropdown.style.display = "block";
    try {
        const data = await api(`/api/fs/browse?path=${encodeURIComponent(parent)}`);
        _renderFsDropdown(data.path, data.dirs, fragment);
    } catch (e) {
        dropdown.innerHTML = `<div class="fs-dir-item fs-empty" style="color:var(--accent-red)">Error: ${e.message}</div>`;
    }
}

function _closeFsDropdown() {
    const dd = document.getElementById("fs-dropdown");
    if (dd) dd.style.display = "none";
    _fsActiveInput = null;
}

function _attachFsDropdown(inputEl) {
    inputEl.addEventListener("focus", () => {
        _fsActiveInput = inputEl;
        _fsBrowsedPath = null;
        _openFsDropdown();
    });
    inputEl.addEventListener("input", () => {
        clearTimeout(_fsBlurTimer);
        _fsActiveInput = inputEl;
        const { parent } = _parseFsPath(inputEl.value.trim() || "/");
        if (parent !== _fsBrowsedPath) _fsBrowsedPath = null;
        _openFsDropdown();
    });
    inputEl.addEventListener("blur", () => {
        // Delay to let the mousedown on items fire before closing
        _fsBlurTimer = setTimeout(_closeFsDropdown, 150);
    });
}

_attachFsDropdown(document.getElementById("cat-save-path"));
_attachFsDropdown(document.getElementById("save-path"));

window.addEventListener("scroll", () => {
    const dd = document.getElementById("fs-dropdown");
    if (dd && dd.style.display !== "none") _positionFsDropdown();
}, true);

function editCategory(name) {
    showCategoryForm(name);
}

// Adopt an orphaned label by defining the category it refers to.
//
// A torrent points at its category by name, so this needs no new endpoint and
// no write to the torrents: the moment a category called the same thing exists,
// every torrent already labelled with it belongs to it again. This matters
// because the only action previously offered on an orphan was Delete, which
// strips the label off all of them -- the opposite of what someone whose
// categories went missing in an upgrade wants.
async function adoptCategory(idx) {
    const o = _orphanInfo[idx];
    if (!o) return;
    await showCategoryForm(); // no argument: create mode, so saveCategory POSTs

    const el = document.getElementById("cat-name");
    if (el) el.value = o.name;

    // Start from what the torrents already do. The form opens on race with an
    // empty path, and accepting that for a hoard set moves every torrent
    // wearing the label to the other engine -- a silent reclassification that
    // only shows up later, when something downstream trips over it.
    const mode = document.getElementById("cat-mode");
    if (mode && o.mode) {
        mode.value = o.mode;
        if (typeof _catModeChanged === "function") _catModeChanged();
    }
    const sp = document.getElementById("cat-save-path");
    if (sp && o.save_path) sp.value = o.save_path;

    const res = document.getElementById("cat-result");
    if (res) {
        res.textContent = t("Filled in from the {n} torrent(s) already labelled {label}: they run in {mode}, mostly under {path}. Check it, then save to adopt them.",
            { n: o.torrents, label: o.name, mode: o.mode || "?", path: o.save_path || "?" });
        res.className = "result-msg";
        res.style.display = "block";
    }
    if (sp) sp.focus();
}

async function saveCategory() {
    const name = document.getElementById("cat-name").value.trim();
    const mode = document.getElementById("cat-mode").value;
    const save_path = document.getElementById("cat-save-path").value.trim();
    const resultEl = document.getElementById("cat-result");

    if (!name || !save_path) {
        resultEl.textContent = t("Name and save path are required");
        resultEl.className = "result-msg error";
        resultEl.style.display = "block";
        return;
    }

    try {
        const placement = [...document.querySelectorAll(".cat-agent-cb:checked")].map(c => c.value);
        const agents = {};
        document.querySelectorAll(".cat-agent-path").forEach(inp => {
            const v = inp.value.trim();
            if (v) agents[inp.dataset.agent] = v;
        });
        const strategy = document.getElementById("cat-strategy").value;
        const minFreeGiB = parseFloat(document.getElementById("cat-min-free").value) || 0;
        const min_free_bytes = Math.max(0, Math.round(minFreeGiB * 1024 * 1024 * 1024));
        const graduate_to = mode === "race" ? (document.getElementById("cat-graduate-to").value || "") : "";
        const payload = { name, save_path, mode, placement, agents, strategy, graduate_to, min_free_bytes };
        if (_editingCategory) {
            await api(`/api/categories/${encodeURIComponent(_editingCategory)}`, {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(payload),
            });
        } else {
            await api("/api/categories", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(payload),
            });
        }
        hideCategoryForm();
        await updateCategories();
    } catch (e) {
        resultEl.textContent = t("Error: {msg}", { msg: e.message });
        resultEl.className = "result-msg error";
        resultEl.style.display = "block";
    }
}

async function deleteCategory(name) {
    if (!await hydraConfirm(t("Delete category \"{name}\"?", { name: name }))) return;
    try {
        await api(`/api/categories/${encodeURIComponent(name)}`, { method: "DELETE" });
        await updateCategories();
    } catch (e) {
        hydraNotify(t("Error: {msg}", { msg: e.message }));
    }
}

// Auto-fill mode/save_path when a category is selected in add-torrent form
document.getElementById("torrent-category").addEventListener("change", async (e) => {
    const name = e.target.value;
    if (!name) return;
    try {
        const cats = await api("/api/categories");
        // /api/categories returns an ARRAY of {name, save_path, mode, ...}.
        const cat = Array.isArray(cats) ? cats.find(c => c.name === name) : (cats || {})[name];
        if (cat) {
            if (cat.save_path) document.getElementById("save-path").value = cat.save_path;
            document.querySelectorAll(".mode-btn").forEach(b => {
                b.classList.toggle("active", b.dataset.mode === cat.mode);
            });
        }
    } catch {}
});

// ─── Benchmark ──────────────────────────────────────────

let _bmCharts = null;
let _bmSpan = 900; // seconds (15m default)

// Per-column metadata: label, formatter, higher_better (true/false/null)
const BM_META = {
    race_upload_rate:        { label: "Upload Race",          fmt: formatSpeed,               hb: true  },
    race_download_rate:      { label: "Download Race",        fmt: formatSpeed,               hb: true  },
    race_peers:              { label: "Peers Race",           fmt: v => v.toFixed(0),         hb: true  },
    race_torrents:           { label: "Torrents Race",        fmt: v => v.toFixed(0),         hb: null  },
    race_uploading:          { label: "Active Upload Race",   fmt: v => v.toFixed(0),         hb: true  },
    hoard_upload_rate:       { label: "Upload Hoard",         fmt: formatSpeed,               hb: true  },
    hoard_peers:             { label: "Peers Hoard",          fmt: v => v.toFixed(0),         hb: true  },
    hoard_active:            { label: "Active Hoard",         fmt: v => v.toFixed(0),         hb: true  },
    hoard_with_peers:        { label: "With Peers Hoard",     fmt: v => v.toFixed(0),         hb: true  },
    hoard_uploading:         { label: "Active Upload Hoard",  fmt: v => v.toFixed(0),         hb: true  },
    iowait_pct:              { label: "IOWait %",             fmt: v => v.toFixed(2) + "%",   hb: false },
    arc_size_bytes:          { label: "ARC Size",             fmt: formatBytes,               hb: null  },
    arc_hit_rate_pct:        { label: "ARC Hit Rate",         fmt: v => v.toFixed(3) + "%",   hb: true  },
    arc_demand_hit_rate_pct: { label: "ARC Demand Hit Rate",  fmt: v => v.toFixed(3) + "%",   hb: true  },
    arc_miss_per_sec:        { label: "ARC Miss/s",           fmt: v => v.toFixed(1),         hb: false },
    arc_demand_miss_per_sec: { label: "ARC Demand Miss/s",    fmt: v => v.toFixed(1),         hb: false },
    arc_ghost_hits_per_sec:  { label: "ARC Ghost Hits/s",     fmt: v => v.toFixed(1),         hb: null  },
};

// Crosshair plugin, vertical line on hover
const crosshairPlugin = {
    id: "crosshair",
    afterDraw(chart) {
        if (chart.tooltip?._active?.length) {
            const x = chart.tooltip._active[0].element.x;
            const yAxis = chart.scales.y;
            const ctx = chart.ctx;
            ctx.save();
            ctx.beginPath();
            ctx.moveTo(x, yAxis.top);
            ctx.lineTo(x, yAxis.bottom);
            ctx.lineWidth = 1;
            ctx.strokeStyle = "rgba(255,255,255,0.2)";
            ctx.stroke();
            ctx.restore();
        }
    }
};

function _fmtDuration(secs) {
    if (secs >= 86400) { const d = secs / 86400; return d >= 2 ? Math.round(d) + "d" : d.toFixed(1) + "d"; }
    if (secs >= 3600) { const h = secs / 3600; return h >= 2 ? Math.round(h) + "h" : h.toFixed(1) + "h"; }
    return Math.round(secs / 60) + "min";
}

function _bmLabel(ts) {
    const d = new Date(ts * 1000);
    return _bmSpan <= 86400 ? d.toLocaleTimeString() : d.toLocaleDateString() + " " + d.toLocaleTimeString([], {hour:"2-digit",minute:"2-digit"});
}

// Dedicated label for the VPN speedtest chart: always day-format (the chart
// covers 7 days at hourly cadence, hour-format makes the X axis unreadable).
function _vpnDayLabel(ts) {
    return new Date(ts * 1000).toLocaleDateString(undefined, { weekday: "short", day: "2-digit", month: "2-digit" });
}
let _vpnHistoryRef = [];

function _mkChart(id, color, yFormatter) {
    return new Chart(document.getElementById(id), {
        type: "line",
        data: {
            labels: [],
            datasets: [{
                data: [],
                borderColor: color,
                backgroundColor: color + "18",
                borderWidth: 1.5,
                pointRadius: 0,
                pointHitRadius: 10,
                pointHoverRadius: 4,
                tension: 0.3,
                fill: true,
            }]
        },
        plugins: [crosshairPlugin],
        options: {
            responsive: true,
            maintainAspectRatio: false,
            animation: false,
            interaction: { mode: "index", intersect: false },
            plugins: {
                legend: { display: false },
                tooltip: {
                    mode: "index",
                    intersect: false,
                    callbacks: {
                        label: ctx => yFormatter ? yFormatter(ctx.parsed.y) : ctx.parsed.y
                    }
                }
            },
            scales: {
                x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0 }, grid: { display: false } },
                y: {
                    beginAtZero: true,
                    grid: { color: "#1a2233" },
                    ticks: { color: "#7a90a8", callback: v => yFormatter ? yFormatter(v) : v }
                }
            }
        }
    });
}

function _mkDualChart(id, label1, color1, label2, color2, yFormatter) {
    return new Chart(document.getElementById(id), {
        type: "line",
        data: {
            labels: [],
            datasets: [
                {
                    label: label1,
                    data: [],
                    borderColor: color1,
                    backgroundColor: color1 + "18",
                    borderWidth: 1.5,
                    pointRadius: 0,
                    pointHitRadius: 10,
                    pointHoverRadius: 4,
                    tension: 0.3,
                    fill: true,
                },
                {
                    label: label2,
                    data: [],
                    borderColor: color2,
                    backgroundColor: color2 + "18",
                    borderWidth: 1.5,
                    pointRadius: 0,
                    pointHitRadius: 10,
                    pointHoverRadius: 4,
                    tension: 0.3,
                    fill: true,
                },
            ],
        },
        plugins: [crosshairPlugin],
        options: {
            responsive: true,
            maintainAspectRatio: false,
            animation: false,
            interaction: { mode: "index", intersect: false },
            plugins: {
                legend: { display: true, labels: { color: "#7a90a8", boxWidth: 12 } },
                tooltip: {
                    mode: "index",
                    intersect: false,
                    callbacks: {
                        label: ctx => `${ctx.dataset.label}: ${yFormatter ? yFormatter(ctx.parsed.y) : ctx.parsed.y}`
                    }
                }
            },
            scales: {
                x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0 }, grid: { display: false } },
                y: {
                    beginAtZero: true,
                    grid: { color: "#1a2233" },
                    ticks: { color: "#7a90a8", callback: v => yFormatter ? yFormatter(v) : v }
                }
            }
        }
    });
}

// _fmtAnnRate renders an announces/second figure: sub-1 rates keep two
// decimals (a small instance announces a few times a minute), larger ones
// round, since 87.4/s and 87/s say the same thing.
function _fmtAnnRate(v) {
    v = v ?? 0;
    if (v > 0 && v < 1) return v.toFixed(2) + "/s";
    return (v >= 10 ? v.toFixed(0) : v.toFixed(1)) + "/s";
}

function _updateDualChart(chart, history, key1, key2) {
    chart.data.labels = history.map(p => _bmLabel(p.ts));
    chart.data.datasets[0].data = history.map(p => p[key1] ?? 0);
    chart.data.datasets[1].data = history.map(p => p[key2] ?? 0);
    chart.update("none");
}

function _initBmCharts() {
    Chart.defaults.color = "#7a90a8";
    _bmCharts = {
        // Debit, donc formatSpeed : il suit le reglage Vitesses (octets/s ou bits/s).
        // formatBytes ne connait que le reglage Tailles et rendait toujours des KiB/s.
        upload:    _mkDualChart("chart-upload", "Race", "#f0883e", "Hoard", "#3fb950", formatSpeed),
        totalUploaded: new Chart(document.getElementById("chart-total-uploaded"), {
            type: "bar",
            data: {
                labels: [],
                datasets: [
                    { label: "Race", data: [], backgroundColor: "#f0883e99", stack: "ul" },
                    { label: "Hoard", data: [], backgroundColor: "#3fb95099", stack: "ul" },
                ],
            },
            plugins: [crosshairPlugin],
            options: {
                responsive: true,
                maintainAspectRatio: false,
                animation: false,
                interaction: { mode: "index", intersect: false },
                plugins: {
                    legend: { display: true, labels: { color: "#7a90a8", boxWidth: 12 } },
                    tooltip: {
                        mode: "index",
                        intersect: false,
                        callbacks: {
                            label: ctx => `${ctx.dataset.label}: ${formatBytes(ctx.parsed.y)}`,
                            afterBody: ctx => {
                                const idx = ctx[0].dataIndex;
                                const total = ctx[0].chart.data.datasets.reduce((s, d) => s + (d.data[idx] || 0), 0);
                                return `\nTotal: ${formatBytes(total)}`;
                            },
                        },
                    },
                },
                scales: {
                    x: { display: true, stacked: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0 }, grid: { display: false } },
                    y: {
                        beginAtZero: true,
                        stacked: true,
                        grid: { color: "#1a2233" },
                        ticks: { color: "#7a90a8", callback: v => formatBytes(v) },
                    },
                },
            },
        }),
        uploading: new Chart(document.getElementById("chart-uploading"), {
            type: "line",
            data: {
                labels: [],
                datasets: [
                    { label: t("Race UL"), data: [], borderColor: "#f0883e", backgroundColor: "#f0883e18", borderWidth: 1.5, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false },
                    { label: t("Hoard UL"), data: [], borderColor: "#3fb950", backgroundColor: "#3fb95018", borderWidth: 1.5, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false },
                    { label: t("Race Peers"), data: [], borderColor: "#bc8cff", backgroundColor: "#bc8cff18", borderWidth: 1, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false, borderDash: [4, 2] },
                    { label: t("Hoard Peers"), data: [], borderColor: "#58a6ff", backgroundColor: "#58a6ff18", borderWidth: 1, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false, borderDash: [4, 2] },
                ],
            },
            plugins: [crosshairPlugin],
            options: {
                responsive: true, maintainAspectRatio: false, animation: false,
                interaction: { mode: "index", intersect: false },
                plugins: {
                    legend: { display: true, labels: { color: "#7a90a8", boxWidth: 12 } },
                    tooltip: { mode: "index", intersect: false },
                },
                scales: {
                    x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0 }, grid: { display: false } },
                    y: { beginAtZero: true, grid: { color: "#1a2233" }, ticks: { color: "#7a90a8" } },
                },
            },
        }),
        announce: new Chart(document.getElementById("chart-announce"), {
            type: "line",
            data: {
                labels: [],
                datasets: [
                    { label: t("Race announces/s"), data: [], borderColor: "#f0883e", backgroundColor: "#f0883e18", borderWidth: 1.5, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false },
                    { label: t("Hoard announces/s"), data: [], borderColor: "#3fb950", backgroundColor: "#3fb95018", borderWidth: 1.5, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false },
                    { label: t("Race failed/s"), data: [], borderColor: "#f85149", backgroundColor: "#f8514918", borderWidth: 1, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false, borderDash: [4, 2] },
                    { label: t("Hoard failed/s"), data: [], borderColor: "#d29922", backgroundColor: "#d2992218", borderWidth: 1, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false, borderDash: [4, 2] },
                ],
            },
            plugins: [crosshairPlugin],
            options: {
                responsive: true, maintainAspectRatio: false, animation: false,
                interaction: { mode: "index", intersect: false },
                plugins: {
                    legend: { display: true, labels: { color: "#7a90a8", boxWidth: 12 } },
                    tooltip: {
                        mode: "index", intersect: false,
                        callbacks: { label: ctx => `${ctx.dataset.label}: ${_fmtAnnRate(ctx.parsed.y)}` },
                    },
                },
                scales: {
                    x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0 }, grid: { display: false } },
                    y: { beginAtZero: true, grid: { color: "#1a2233" }, ticks: { color: "#7a90a8", callback: v => _fmtAnnRate(v) } },
                },
            },
        }),
        raceEvents: new Chart(document.getElementById("chart-share"), {
            type: "bar",
            data: {
                labels: [],
                datasets: [
                    { label: "Added", data: [], backgroundColor: "#58a6ff99", stack: "ev" },
                    { label: "Completed", data: [], backgroundColor: "#3fb95099", stack: "ev" },
                    { label: "1st Upload", data: [], backgroundColor: "#f0883e99", stack: "ev" },
                ],
            },
            plugins: [crosshairPlugin],
            options: {
                responsive: true, maintainAspectRatio: false, animation: false,
                interaction: { mode: "index", intersect: false },
                plugins: {
                    legend: { display: true, labels: { color: "#7a90a8", boxWidth: 12 } },
                    tooltip: { mode: "index", intersect: false },
                },
                scales: {
                    x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 6, maxRotation: 0 }, grid: { display: false } },
                    y: { beginAtZero: true, stacked: true, grid: { color: "#1a2233" }, ticks: { color: "#7a90a8", stepSize: 1 } },
                },
            },
        }),
        iowait:    _mkChart("chart-iowait",    "#f85149", v => v.toFixed(1) + "%"),
        arcMiss:   _mkChart("chart-arc-miss",  "#e3b341", v => v.toFixed(1)),
        vpn: new Chart(document.getElementById("chart-vpn"), {
            type: "line",
            data: {
                labels: [],
                datasets: [
                    {
                        label: "Upload",
                        data: [],
                        borderColor: "#f0883e",
                        backgroundColor: "#f0883e18",
                        borderWidth: 1.5,
                        pointRadius: 4,
                        pointHitRadius: 10,
                        pointHoverRadius: 6,
                        tension: 0.2,
                        fill: false,
                        spanGaps: false,
                    },
                    {
                        label: "Download",
                        data: [],
                        borderColor: "#58a6ff",
                        backgroundColor: "#58a6ff18",
                        borderWidth: 1.5,
                        pointRadius: 4,
                        pointHitRadius: 10,
                        pointHoverRadius: 6,
                        tension: 0.2,
                        fill: false,
                        spanGaps: false,
                    },
                ],
            },
            plugins: [crosshairPlugin],
            options: {
                responsive: true,
                maintainAspectRatio: false,
                animation: false,
                interaction: { mode: "index", intersect: false },
                plugins: {
                    legend: { display: true, labels: { color: "#7a90a8", boxWidth: 12 } },
                    tooltip: {
                        mode: "index",
                        intersect: false,
                        callbacks: {
                            title: items => {
                                const ts = _vpnHistoryRef[items[0]?.dataIndex]?.ts;
                                return ts ? new Date(ts * 1000).toLocaleString() : "";
                            },
                            label: ctx => `${ctx.dataset.label}: ${ctx.parsed.y?.toFixed(1) ?? "-"} Mbps`,
                        },
                    },
                },
                scales: {
                    x: { display: true, ticks: { color: "#7a90a8", maxTicksLimit: 7, maxRotation: 0, autoSkip: true }, grid: { display: false } },
                    y: {
                        beginAtZero: true,
                        grid: { color: "#1a2233" },
                        ticks: { color: "#7a90a8", callback: v => v.toFixed(0) + " M" },
                    },
                },
            },
        }),
    };
}

function _updateChart(chart, history, key) {
    chart.data.labels = history.map(p => _bmLabel(p.ts));
    chart.data.datasets[0].data = history.map(p => p[key] ?? 0);
    chart.update("none");
}

// Range selector
document.querySelectorAll(".bm-range-btn").forEach(btn => {
    btn.addEventListener("click", () => {
        document.querySelectorAll(".bm-range-btn").forEach(b => b.classList.remove("active"));
        btn.classList.add("active");
        _bmSpan = parseInt(btn.dataset.span);
        updateBenchmark();
    });
});

async function updateBenchmark() {
    try {
        const now = Date.now() / 1000;
        const start = now - _bmSpan;

        const [cur, history] = await Promise.all([
            api("/api/benchmark/current"),
            api(`/api/benchmark/range?start=${start}&end=${now}`),
        ]);

        if (!_bmCharts) _initBmCharts();

        // Live cards
        const totalUpload = (cur.race_upload_rate ?? 0) + (cur.hoard_upload_rate ?? 0);
        document.getElementById("bm-race-upload").textContent   = formatSpeed(cur.race_upload_rate ?? 0);
        document.getElementById("bm-race-download").textContent = formatSpeed(cur.race_download_rate ?? 0);
        document.getElementById("bm-hoard-upload").textContent  = formatSpeed(cur.hoard_upload_rate ?? 0);
        document.getElementById("bm-race-peers").textContent    = (cur.race_peers ?? 0).toFixed(0);
        document.getElementById("bm-iowait").textContent        = (cur.iowait_pct ?? 0).toFixed(1) + "%";
        document.getElementById("bm-arc-hit").textContent       = (cur.arc_hit_rate_pct ?? 0).toFixed(3) + "%";
        document.getElementById("bm-arc-miss").textContent      = (cur.arc_miss_per_sec ?? 0).toFixed(1) + "/s";
        document.getElementById("bm-arc-size").textContent      = formatBytes(cur.arc_size_bytes ?? 0);

        // Range hint
        document.getElementById("bm-range-hint").textContent =
            history.length > 0
                ? t("{n} points · {from} → {to}", {
                    n: history.length,
                    from: new Date(history[0].ts * 1000).toLocaleString(),
                    to: new Date(history[history.length-1].ts * 1000).toLocaleString() })
                : t("No data for this range");

        // Charts
        _updateDualChart(_bmCharts.upload, history, "race_upload_rate", "hoard_upload_rate");

        // Total uploaded, 20 stacked bars, each = sum of volume in bucket
        {
            const N = 20;
            if (history.length >= 2) {
                const tMin = history[0].ts, tMax = history[history.length - 1].ts;
                const bucketSize = (tMax - tMin) / N;
                const durEl = document.getElementById("bm-bar-duration-uploaded");
                if (durEl) durEl.textContent = "(" + _fmtDuration(bucketSize) + "/bar)";
                const labels = [], raceVol = new Float64Array(N), hoardVol = new Float64Array(N);
                for (let b = 0; b < N; b++) {
                    const mid = tMin + bucketSize * (b + 0.5);
                    labels.push(_bmLabel(mid));
                }
                for (let i = 1; i < history.length; i++) {
                    const dt = history[i].ts - history[i - 1].ts;
                    const b = Math.min(Math.floor((history[i].ts - tMin) / bucketSize), N - 1);
                    raceVol[b] += (history[i].race_upload_rate ?? 0) * dt;
                    hoardVol[b] += (history[i].hoard_upload_rate ?? 0) * dt;
                }
                _bmCharts.totalUploaded.data.labels = labels;
                _bmCharts.totalUploaded.data.datasets[0].data = Array.from(raceVol);
                _bmCharts.totalUploaded.data.datasets[1].data = Array.from(hoardVol);
            } else {
                _bmCharts.totalUploaded.data.labels = [];
                _bmCharts.totalUploaded.data.datasets[0].data = [];
                _bmCharts.totalUploaded.data.datasets[1].data = [];
            }
            _bmCharts.totalUploaded.update("none");
        }

        // Currently Uploading + Peers (4 datasets)
        {
            const c = _bmCharts.uploading;
            c.data.labels = history.map(p => _bmLabel(p.ts));
            c.data.datasets[0].data = history.map(p => p.race_uploading ?? 0);
            c.data.datasets[1].data = history.map(p => p.hoard_uploading ?? 0);
            c.data.datasets[2].data = history.map(p => p.race_peers ?? 0);
            c.data.datasets[3].data = history.map(p => p.hoard_peers ?? 0);
            c.update("none");
        }
        // Announce rate (4 datasets: per-engine cadence + failures)
        {
            const c = _bmCharts.announce;
            c.data.labels = history.map(p => _bmLabel(p.ts));
            c.data.datasets[0].data = history.map(p => p.race_announce_rate ?? 0);
            c.data.datasets[1].data = history.map(p => p.hoard_announce_rate ?? 0);
            c.data.datasets[2].data = history.map(p => p.race_announce_fail_rate ?? 0);
            c.data.datasets[3].data = history.map(p => p.hoard_announce_fail_rate ?? 0);
            c.update("none");
        }
        // Race Events, 20 stacked bars (added, completed, first_upload)
        {
            const N = 20;
            if (history.length >= 2) {
                const tMin = history[0].ts, tMax = history[history.length - 1].ts;
                const bucketSize = (tMax - tMin) / N;
                const durEl = document.getElementById("bm-bar-duration-events");
                if (durEl) durEl.textContent = "(" + _fmtDuration(bucketSize) + "/bar)";
                try {
                    const events = await api(`/api/benchmark/race-events?start=${tMin}&end=${tMax}`);
                    const labels = [];
                    const added = new Float64Array(N), completed = new Float64Array(N), firstUl = new Float64Array(N);
                    for (let b = 0; b < N; b++) labels.push(_bmLabel(tMin + bucketSize * (b + 0.5)));
                    if (events) for (const ev of events) {
                        const b = Math.min(Math.floor((ev.ts - tMin) / bucketSize), N - 1);
                        if (b < 0) continue;
                        if (ev.event === "added") added[b]++;
                        else if (ev.event === "completed") completed[b]++;
                        else if (ev.event === "first_upload") firstUl[b]++;
                    }
                    _bmCharts.raceEvents.data.labels = labels;
                    _bmCharts.raceEvents.data.datasets[0].data = Array.from(added);
                    _bmCharts.raceEvents.data.datasets[1].data = Array.from(completed);
                    _bmCharts.raceEvents.data.datasets[2].data = Array.from(firstUl);
                } catch(e) {
                    _bmCharts.raceEvents.data.labels = [];
                    _bmCharts.raceEvents.data.datasets.forEach(d => d.data = []);
                }
            } else {
                _bmCharts.raceEvents.data.labels = [];
                _bmCharts.raceEvents.data.datasets.forEach(d => d.data = []);
            }
            _bmCharts.raceEvents.update("none");
        }
        _updateChart(_bmCharts.iowait, history, "iowait_pct");
        _updateChart(_bmCharts.arcMiss, history, "arc_miss_per_sec");

        // VPN speedtest, 7-day window, axis in days
        const [vpnLatest, vpnHistory] = await Promise.all([
            api("/api/vpn-speedtest/latest").catch(() => null),
            api("/api/vpn-speedtest/history?hours=168").catch(() => []),
        ]);
        if (vpnLatest && vpnLatest.ts) {
            document.getElementById("vpn-ul").textContent = (vpnLatest.ul_mbps ?? 0).toFixed(1) + " Mbps";
            document.getElementById("vpn-dl").textContent = (vpnLatest.dl_mbps ?? 0).toFixed(1) + " Mbps";
            document.getElementById("vpn-ul-total").textContent = ((vpnLatest.ul_mbps ?? 0) + (vpnLatest.ul_torrent_mbps ?? 0)).toFixed(1) + " Mbps";
            document.getElementById("vpn-dl-total").textContent = ((vpnLatest.dl_mbps ?? 0) + (vpnLatest.dl_torrent_mbps ?? 0)).toFixed(1) + " Mbps";
            document.getElementById("vpn-ts").textContent = new Date(vpnLatest.ts * 1000).toLocaleString();
        }
        if (_bmCharts.vpn && vpnHistory.length > 0) {
            _vpnHistoryRef = vpnHistory;
            _bmCharts.vpn.data.labels = vpnHistory.map(p => _vpnDayLabel(p.ts));
            // Interpolate 0 values with previous point, mark them
            const ulData = [], dlData = [], ulColors = [], dlColors = [];
            const ulBase = "#f0883e", dlBase = "#58a6ff", interpColor = "#f8514988";
            let prevUl = null, prevDl = null;
            for (const p of vpnHistory) {
                if (p.ul_mbps > 0) { prevUl = p.ul_mbps; ulData.push(p.ul_mbps); ulColors.push(ulBase); }
                else { ulData.push(prevUl); ulColors.push(interpColor); }
                if (p.dl_mbps > 0) { prevDl = p.dl_mbps; dlData.push(p.dl_mbps); dlColors.push(dlBase); }
                else { dlData.push(prevDl); dlColors.push(interpColor); }
            }
            _bmCharts.vpn.data.datasets[0].data = ulData;
            _bmCharts.vpn.data.datasets[0].pointBackgroundColor = ulColors;
            _bmCharts.vpn.data.datasets[1].data = dlData;
            _bmCharts.vpn.data.datasets[1].pointBackgroundColor = dlColors;
            _bmCharts.vpn.update("none");
        }

    } catch (e) {
        console.error("Benchmark update error:", e);
    }
}

async function runVpnSpeedtest() {
    const btn = document.getElementById("vpn-run-btn");
    btn.disabled = true;
    btn.textContent = "⏳ " + t("Running…");
    try {
        const result = await api("/api/vpn-speedtest/run", { method: "POST" });
        document.getElementById("vpn-ul").textContent = (result.ul_mbps ?? 0).toFixed(1) + " Mbps";
        document.getElementById("vpn-dl").textContent = (result.dl_mbps ?? 0).toFixed(1) + " Mbps";
        document.getElementById("vpn-ul-total").textContent = ((result.ul_mbps ?? 0) + (result.ul_torrent_mbps ?? 0)).toFixed(1) + " Mbps";
        document.getElementById("vpn-dl-total").textContent = ((result.dl_mbps ?? 0) + (result.dl_torrent_mbps ?? 0)).toFixed(1) + " Mbps";
        document.getElementById("vpn-ts").textContent = new Date(result.ts * 1000).toLocaleString();
    } catch (e) {
        console.error("VPN speedtest error:", e);
    } finally {
        btn.disabled = false;
        btn.textContent = "▶ " + t("Run now");
    }
}

// ─── Comparaison A/B ────────────────────────────────────

async function runCompare() {
    const startVal = document.getElementById("cmp-start").value;
    const midVal   = document.getElementById("cmp-mid").value;
    const endVal   = document.getElementById("cmp-end").value;
    const errEl    = document.getElementById("cmp-error");
    const resEl    = document.getElementById("cmp-result");

    errEl.style.display = "none";
    resEl.style.display = "none";

    if (!startVal || !midVal || !endVal) {
        errEl.textContent = t("Fill in all three dates.");
        errEl.style.display = "block";
        return;
    }

    const start = new Date(startVal).getTime() / 1000;
    const mid   = new Date(midVal).getTime() / 1000;
    const end   = new Date(endVal).getTime() / 1000;

    if (start >= mid || mid >= end) {
        errEl.textContent = t("Dates must be in order: start < middle < end.");
        errEl.style.display = "block";
        return;
    }

    try {
        const data = await api(`/api/benchmark/compare?start=${start}&mid=${mid}&end=${end}`);

        if (!data.metrics) {
            errEl.textContent = t("No data for this range.");
            errEl.style.display = "block";
            return;
        }

        document.getElementById("cmp-counts").textContent =
            t("P1: {p1} samples, P2: {p2} samples", { p1: data.p1_count, p2: data.p2_count });

        const tbody = document.getElementById("cmp-tbody");
        tbody.innerHTML = Object.entries(data.metrics).map(([col, v]) => {
            const meta = BM_META[col] || { label: col, fmt: x => x.toFixed(2), hb: null };
            const delta = v.delta_avg_pct;
            const improved = meta.hb === true ? delta > 0 : meta.hb === false ? delta < 0 : null;
            const deltaColor = improved === null ? "var(--text-secondary)"
                             : improved           ? "var(--accent-green)"
                             :                      "var(--accent-red)";
            const arrow = improved === null ? "" : improved ? " ▲" : " ▼";
            const sign  = delta > 0 ? "+" : "";
            return `<tr>
                <td class="cmp-col-label">${t(meta.label)}</td>
                <td>${meta.fmt(v.p1.avg)}</td>
                <td class="cmp-secondary">${meta.fmt(v.p1.max)}</td>
                <td>${meta.fmt(v.p2.avg)}</td>
                <td class="cmp-secondary">${meta.fmt(v.p2.max)}</td>
                <td style="color:${deltaColor};font-weight:700">${sign}${delta.toFixed(1)}%${arrow}</td>
            </tr>`;
        }).join("");

        resEl.style.display = "block";
    } catch (e) {
        errEl.textContent = t("Error: {msg}", { msg: e.message });
        errEl.style.display = "block";
    }
}

// ─── Metrics uptime ─────────────────────────────────────

// ─── Race drain policy panel (Phase 1) ─────────────────
async function loadRacePolicy() {
    let st;
    try { st = await api("/api/drain/status"); } catch (e) { return; }
    const pctEl = document.getElementById("rp-disk-pct");
    if (!pctEl) return;
    const pct = st.disk_used_pct || 0;
    const free = (st.disk_total || 0) - (st.disk_used || 0);
    pctEl.textContent = pct.toFixed(0) + "%";
    document.getElementById("rp-disk-free").textContent = formatBytes(free);
    document.getElementById("rp-fill").style.width = Math.min(100, pct) + "%";
    document.getElementById("rp-mark-low").style.left = (st.low_watermark || 0) + "%";
    document.getElementById("rp-mark-high").style.left = (st.high_watermark || 0) + "%";
    _rpSet("rp-enabled", st.enabled, true);
    _rpSet("rp-high", st.high_watermark);
    _rpSet("rp-low", st.low_watermark);
    _rpSet("rp-minage", st.min_age_minutes);
    _rpSet("rp-interval", st.check_interval);
    _rpSet("rp-maxage", st.max_age_hours);
    _rpSet("rp-minratio", st.min_ratio);
    if (!_rpDirty.has("rp-enabled")) {
        const _bd = document.getElementById("rp-body-drain");
        if (_bd) _bd.classList.toggle("off", !st.enabled);
    }
    if (!_rpDirty.has("rp-ar-enabled")) {
        const _are = document.getElementById("rp-ar-enabled");
        if (_are) _are.checked = !!st.age_ratio_enabled;
        const _bar = document.getElementById("rp-body-ar");
        if (_bar) _bar.classList.toggle("off", !st.age_ratio_enabled);
    }
    _rpUpdateDrainBtn();
    const _mode = (st.age_ratio_mode === "or") ? "or" : "and";
    const _ea = document.getElementById("rp-mode-and"), _eo = document.getElementById("rp-mode-or");
    if (_ea && _eo && !_rpDirty.has("__mode__")) { _ea.classList.toggle("on", _mode === "and"); _eo.classList.toggle("on", _mode === "or"); }
    const _act = (st.age_ratio_action === "hoard") ? "hoard" : "delete";
    const _ad = document.getElementById("rp-act-delete"), _ah = document.getElementById("rp-act-hoard");
    if (_ad && _ah && !_rpDirty.has("__action__")) { _ad.classList.toggle("on", _act === "delete"); _ah.classList.toggle("on", _act === "hoard"); }
    _rpLoadGraduations();
}
async function _rpLoadGraduations() {
    const box = document.getElementById("rp-grad");
    if (!box) return;
    let g;
    try { g = await api("/api/drain/graduations"); } catch (e) { return; }
    if (!g || !g.length) { box.innerHTML = ""; return; }
    box.innerHTML = '<div class="rp2-grad-title">Graduating to hoard</div>' + g.map(x => {
        const pct = Math.min(100, x.pct || 0);
        return `<div class="rp2-grad-row"><div class="rp2-grad-name" title="${esc(x.name || "")}">${esc(x.name || x.info_hash)}</div>`
            + `<div class="rp2-grad-bar"><div class="rp2-grad-fill" style="width:${pct.toFixed(1)}%"></div></div>`
            + `<div class="rp2-grad-pct">${formatBytes(x.copied || 0)} / ${formatBytes(x.total || 0)}</div></div>`;
    }).join("");
}
function _rpSetMode(m) {
    document.getElementById("rp-mode-and").classList.toggle("on", m === "and");
    document.getElementById("rp-mode-or").classList.toggle("on", m === "or");
    _rpSave("age_ratio_mode", m);
}
function _rpSaveEnabled(on) {
    _rpDirty.add("rp-enabled");
    const b = document.getElementById("rp-body-drain");
    if (b) b.classList.toggle("off", !on);
    _rpUpdateDrainBtn();
    _rpSave("enabled", on);
}
// Drain now only does something when a policy is on; grey it out otherwise.
// A threshold of 0 means "no constraint on that axis", not "off", so 0/0 is a
// working policy that matches everything past the min-age floor.
function _rpUpdateDrainBtn() {
    const b = document.getElementById("rp-drain-now");
    if (!b) return;
    const en = document.getElementById("rp-enabled");
    const ar = document.getElementById("rp-ar-enabled");
    const any = (en && en.checked) || (ar && ar.checked);
    b.disabled = !any;
    b.title = any ? "" : t("Enable a policy first");
    _rpUpdateUnbounded();
}
// An unconstrained policy (both thresholds 0) whose action is Delete erases
// every race torrent past the floor. That is irreversible, so say it plainly
// before it runs rather than after.
function _rpUnboundedDelete() {
    const ar = document.getElementById("rp-ar-enabled");
    if (!ar || !ar.checked) return false;
    const num = id => { const e = document.getElementById(id); return e ? parseFloat(e.value) || 0 : 0; };
    if (num("rp-maxage") > 0 || num("rp-minratio") > 0) return false;
    const del = document.getElementById("rp-act-delete");
    return !!(del && del.classList.contains("on"));
}
function _rpUpdateUnbounded() {
    const w = document.getElementById("rp-warn");
    if (!w) return;
    if (_rpUnboundedDelete()) {
        w.textContent = t("Both thresholds are 0, so every race torrent past the keep floor matches, and the action is Delete. Their data will be erased.");
        w.style.display = "";
    } else {
        w.style.display = "none";
    }
}
function _rpSaveAR(on) {
    _rpDirty.add("rp-ar-enabled");
    const b = document.getElementById("rp-body-ar");
    if (b) b.classList.toggle("off", !on);
    _rpUpdateDrainBtn();
    _rpSave("age_ratio_enabled", on);
}
function _rpSetAction(a) {
    document.getElementById("rp-act-delete").classList.toggle("on", a === "delete");
    document.getElementById("rp-act-hoard").classList.toggle("on", a === "hoard");
    _rpSave("age_ratio_action", a);
    _rpUpdateUnbounded();
}
let _rpDirty = new Set();
function _rpSet(id, val, isCheck) {
    const el = document.getElementById(id);
    // Race settings are restart-required, so drain/status still reports the OLD
    // value until a restart. Once the user edits a field, stop the poll from
    // clobbering their pending choice (until page reload).
    if (!el || el === document.activeElement || _rpDirty.has(id)) return;
    if (isCheck) el.checked = !!val; else el.value = val;
}
async function _rpSave(key, value) {
    const _idmap = { high_watermark_pct: "rp-high", low_watermark_pct: "rp-low", min_age_minutes: "rp-minage", check_interval_seconds: "rp-interval", enabled: "rp-enabled", max_age_hours: "rp-maxage", min_ratio: "rp-minratio", add_block_enabled: "rp-block", reserve_free_gb: "rp-reserve" };
    if (_idmap[key]) _rpDirty.add(_idmap[key]);
    if (key === "age_ratio_mode") _rpDirty.add("__mode__");
    if (key === "age_ratio_action") _rpDirty.add("__action__");
    try {
        await api("/api/settings", { method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ changes: [{ section: "race_drain", key, value }] }) });
        document.getElementById("rp-apply").style.display = "";
    } catch (e) { hydraNotify(t("Save failed: {msg}", { msg: e.message })); }
    // Thresholds decide whether the age/ratio policy can fire at all, so the
    // Drain now button has to be re-evaluated after any field edit too.
    _rpUpdateDrainBtn();
}
async function _rpRestart() {
    if (!await hydraConfirm(t("Restart Hydra now to apply the race drain settings?"))) return;
    try { await api("/api/settings/restart", { method: "POST" }); } catch (e) {}
}
async function _rpDrainNow(btn) {
    if (_rpUnboundedDelete() &&
        !await hydraConfirm(t("Both thresholds are 0, so this deletes EVERY race torrent older than the keep floor, data included.") + "\n\n" + t("Run it?"))) return;
    btn.disabled = true; const t = btn.textContent; btn.textContent = window.t("Draining…");
    let msg;
    try {
        const r = await api("/api/drain/now", { method: "POST" });
        msg = _rpDrainMsg(r);
    } catch (e) {
        msg = window.t("Failed: {msg}", { msg: e && e.message ? e.message : window.t("unknown error") });
    }
    btn.disabled = false; btn.textContent = t;
    _rpUpdateDrainBtn();
    _rpDrainResult(msg);
    loadRacePolicy();
}
// Turn a /api/drain/now response into one line. A drain that legitimately did
// nothing must still say so, reporting nothing is what made the button look
// dead even when it had run.
function _rpDrainMsg(r) {
    if (!r || typeof r !== "object") return t("Nothing to do.");
    if (r.status === "no_match") {
        if (r.no_category_link)
            return t("{n} torrent(s) matched but their category has no hoard category linked, nothing moved.", { n: r.no_category_link });
        if (r.failed) return t("{n} torrent(s) matched but failed, see the logs.", { n: r.failed });
        return t("Nothing matched the thresholds.");
    }
    if (r.status === "no_drain_needed") return t("Nothing to do: disk is below the start mark.");
    const n = r.removed_count || 0;
    if (!n) return t("Nothing to do.");
    let s = r.action === "hoard"
        ? t("{n} torrent(s) graduated, {size}.", { n: n, size: formatBytes(r.freed || 0) })
        : t("{n} torrent(s) removed, {size}.", { n: n, size: formatBytes(r.freed || 0) });
    if (r.no_category_link) s += " " + t("{n} skipped (no linked category).", { n: r.no_category_link });
    return s;
}
function _rpDrainResult(msg) {
    const el = document.getElementById("rp-drain-result");
    if (!el) { console.log("[drain]", msg); return; }
    el.textContent = msg;
    el.style.display = "";
}
async function _rpToggleHist() {
    const box = document.getElementById("rp-hist");
    if (box.style.display !== "none") { box.style.display = "none"; return; }
    let h;
    try { h = await api("/api/drain/history"); } catch (e) { h = []; }
    if (h && h.length) {
        box.innerHTML = "<table><thead><tr><th>When</th><th>Before</th><th>After</th><th>Freed</th><th>Removed</th></tr></thead><tbody>" +
            h.map(d => `<tr><td>${new Date((d.timestamp || 0) * 1000).toLocaleString()}</td><td>${d.before_pct ?? 0}%</td><td>${d.after_pct ?? 0}%</td><td>${formatBytes(d.freed || 0)}</td><td>${d.removed_count || 0}</td></tr>`).join("") +
            "</tbody></table>";
    } else {
        box.innerHTML = '<div class="rp-muted" style="font-size:12px">No drains yet.</div>';
    }
    box.style.display = "";
}

async function updateUptime() {
    try {
        const res = await fetch("/metrics");
        const text = await res.text();
        const match = text.match(/hydra_uptime_seconds (\d+)/);
        if (match) {
            document.getElementById("uptime-value").textContent = formatUptime(parseInt(match[1]));
        }
    } catch {}
}

// ─── Polling loop ───────────────────────────────────────

// Single-flighted: skips if a previous tick is still in-flight (avoids
// stacking under bad wifi / slow API). Status + hoard stats now flow through
// SSE (status_snapshot, hoard_stats_snapshot), this loop only handles the
// per-tab heavy fetches and /health.
let _polling = false;
async function poll() {
    if (_polling) return;
    _polling = true;
    try {
        await checkHealth();
        const activeTab = document.querySelector(".tab.active")?.dataset.tab;
        if (activeTab === "race") await updateRaceTorrents();
        if (activeTab === "hoard") await updateHoardStats();
        if (activeTab === "categories") await updateCategories();
        if (activeTab === "benchmark") await updateBenchmark();
        if (activeTab === "trackers") { await updateTrackers(); loadTrackerStats(); }
    } finally {
        _polling = false;
    }
}

// Tab change triggers immediate data fetch
document.querySelectorAll(".tab").forEach(tab => {
    tab.addEventListener("click", async () => {
        const t = tab.dataset.tab;
        if (t === "race") await updateRaceTorrents();
        if (t === "hoard") await updateHoardStats();
        if (t === "categories") await updateCategories();
        if (t === "benchmark") await updateBenchmark();
    });
});

// ─── Startup restore polling ─────────────────────────────────
function _startNormalPolling() {
    // Immediate header paint while SSE is establishing.
    updateOverview();
    // poll() already fetches what the active tab needs, categories included.
    // Calling updateCategories() unconditionally on top of it made every page
    // load pay for the category rollups even with the categories screen shut --
    // and on a large library that is the longest request there is, so landing
    // on Trackers left the tracker list and Add torrent waiting behind it on
    // the store's single connection.
    poll();
    // Front-only drops the header stat, and with it the reason to ask a
    // controller for an egress address it never announces from.
    if (document.getElementById("net-poly")) {
        updateNetPoly();
        setInterval(updateNetPoly, 30 * 1000);
    }
    if (document.getElementById("header-exit-ip")) {
        fetchPublicIp();
        setInterval(fetchPublicIp, 2 * 60 * 1000);
    }
    fetchPortForward();
    setInterval(poll, POLL_INTERVAL);
    setInterval(fetchPortForward, 60 * 1000);
    setupHoardSSE();
}

async function _checkStartup() {
    // In store-repair mode there are no engines behind the API and /api/startup
    // is not registered at all. Retrying it forever would spin, and the overlay
    // it drives would sit on top of the explanation the user needs.
    if (STORE_REPAIR_MODE) return;
    const overlay = document.getElementById("startup-overlay");
    try {
        const r = await fetch("/api/startup", { headers: { "X-Api-Key": API_KEY } });
        if (!r.ok) throw new Error();
        const d = await r.json();

        if (d.total > 0) {
            document.getElementById("startup-phase").textContent = t("Restoring state…");
            document.getElementById("startup-restored").textContent = d.restored.toLocaleString();
            document.getElementById("startup-total").textContent = d.total.toLocaleString();
            const pct = Math.min(100, Math.round((d.restored / d.total) * 100));
            document.getElementById("startup-bar").style.width = pct + "%";
        }

        if (d.ready) {
            if (overlay) {
                overlay.classList.add("fade-out");
                setTimeout(() => overlay.remove(), 750);
            }
            _startNormalPolling();
            return;
        }
    } catch (_) {
        // API not ready yet, retrying
    }
    setTimeout(_checkStartup, 500);
}

_checkStartup();


// ---- Hydra SSE live updates (2026-04-19) ----
// Push-based live stats from Typhon via /api/events. Replaces part of the
// 3-second poll for the hoard table: upload_rate / download_rate / peers /
// state are updated in-place within ~1s of the engine emitting them.
// The periodic poll keeps running at POLL_INTERVAL for everything else
// (categories, session stats, tracker errors, etc.).
const _STATUS_TO_STATE = { 0: "paused", 1: "checking_files", 2: "downloading", 3: "seeding" };
let _sseConn = null;
let _syncCursor = 0; // last server_ts seen; echoed as ?since= for delta reconnects
let _resyncing = false; // tab-return full re-hydration in progress
let _resyncSeen = null;
let _hydMap = null; // persistent upsert map during a hydration stream (avoids O(N^2))

// A row is identified by node AND hash. The same torrent on two nodes is two
// rows, which is the whole point of duplicating one; keyed by hash alone the
// second simply replaced the first.
function _rowKey(t) { return _agentKey(t.agent) + "|" + t.info_hash; }

// _agentKey folds every name this node answers to onto one key. Hydration rows
// carry "local-hoard" while a live torrent_added frame carries nothing: keyed
// literally, the same torrent would appear twice in the table, once per frame
// that mentioned it.
function _agentKey(name) { return _isLocalAgent(name) ? "local" : name; }

function setupHoardSSE() {
    if (_sseConn) return;
    // On a reconnect where we already hold the hoard list (e.g. returning to a
    // backgrounded tab, which closed the SSE), skip server re-hydration with
    // hydrate=0: the header still refreshes via the immediate snapshot and live
    // events resume, without re-streaming ~100k rows on every tab focus.
    const _q = [];
    if (API_KEY) _q.push("apikey=" + encodeURIComponent(API_KEY));
    if (_syncCursor && Array.isArray(_hoardAllTorrents) && _hoardAllTorrents.length > 0) _q.push("since=" + _syncCursor);
    const url = "/api/events" + (_q.length ? ("?" + _q.join("&")) : "");
    try {
        _sseConn = new EventSource(url);
    } catch (e) {
        console.warn("SSE not available:", e);
        return;
    }
    _sseConn.onmessage = (ev) => {
        if (document.hidden) return;
        let payload;
        try { payload = JSON.parse(ev.data); } catch (_) { return; }
        const type = payload.event;
        const data = payload.data || {};
        if (type === "sync") {
            // full = server re-hydrates the whole list (cursor too old / first
            // load); drop what we hold so stale rows get pruned by the restream.
            if (data.mode === "full") {
                // Resync (tab-return / cursor too old): keep current rows to
                // avoid a blank flash, upsert the fresh full stream, then prune
                // rows not seen once done. Fixes surviving rows staying stale
                // (id instead of name, no category) after a delta-only reconnect.
                _resyncing = true;
                _resyncSeen = new Set();
            }
            return;
        }
        if (type === "status_snapshot") {
            if (data.server_ts) _syncCursor = data.server_ts;
            try { _renderStatus(data); } catch (_) {}
            return;
        }
        if (type === "hoard_stats_snapshot") {
            try { _renderHoardStatsHeader(data); _hoardStatsPainted = true; } catch (_) {}
            return;
        }
        if (type === "torrent_batch") {
            // Progressive hydration of the hoard list (replaces the ~76MB REST
            // fetch). Race keeps its own on-view fetch (small list), so ignore it.
            if (data.mode !== "hoard") return;
            // A partial batch carries the remote agents' rows, re-pushed every
            // few seconds because no agent event reaches this node's hub. It is
            // a subset of the list, so it upserts and nothing more: it must
            // never end a hydration or prune the rows it does not carry.
            if (data.partial) {
                const rows = Array.isArray(data.torrents) ? data.torrents : [];
                if (!rows.length) return;
                for (const t of rows) {
                    if (t.total_size > 0 && t.total_done > 0) t.ratio = t.total_upload / t.total_done;
                }
                if (_hydMap) {
                    // Hydration in flight: feed its accumulator instead, or the
                    // batch we are in the middle of would overwrite these.
                    for (const t of rows) {
                        _hydMap.set(t.info_hash, t);
                        if (_resyncing) _resyncSeen.add(t.info_hash);
                    }
                    return;
                }
                const byHash = new Map(_hoardAllTorrents.map(t => [t.info_hash, t]));
                let fresh = false;
                for (const t of rows) {
                    if (!byHash.has(t.info_hash)) fresh = true;
                    byHash.set(t.info_hash, t);
                }
                _hoardAllTorrents = Array.from(byHash.values());
                if (fresh) _refreshHoardPins().then(() => { try { _renderHoardCounts(); } catch (_) {} });
                _scheduleHoardRender();
                return;
            }
            // Accumulate into a persistent map (O(1)/row) and materialize the
            // array + counts + render ONCE at done. Rebuilding map+array per
            // batch was O(N^2) => ~24s at 106k though the server streams in <1s.
            if (!_hydMap) _hydMap = new Map(_hoardAllTorrents.map(t => [_rowKey(t), t]));
            if (Array.isArray(data.torrents) && data.torrents.length) {
                for (const t of data.torrents) {
                    // Ratio against data-held (matches the detail panel). The
                    // server sends upload/download, which is 0 for our own
                    // uploads (download==0); recompute at ingest so the list
                    // and the sort agree with the detail.
                    if (t.total_size > 0 && t.total_done > 0) t.ratio = t.total_upload / t.total_done;
                    _hydMap.set(_rowKey(t), t);
                    if (_resyncing) _resyncSeen.add(_rowKey(t));
                }
            }
            if (data.done) {
                if (_resyncing) {
                    // Prune rows removed while we were away.
                    for (const h of Array.from(_hydMap.keys())) {
                        if (!_resyncSeen.has(h)) _hydMap.delete(h);
                    }
                    _resyncing = false; _resyncSeen = null;
                }
                _hoardAllTorrents = Array.from(_hydMap.values());
                _hydMap = null;
                _refreshHoardPins().then(() => { try { _renderHoardCounts(); } catch (_) {} });
                _scheduleHoardRender();
            }
            return;
        }
        if (type === "torrent_added" && data.info_hash) {
            if (_hoardAllTorrents && !_hoardAllTorrents.some(
                    t => t.info_hash === data.info_hash && _isLocalAgent(t.agent))) {
                _hoardAllTorrents.unshift(data); // newest first; dynamic fields fill via stats_snapshot
                _refreshHoardPins().then(() => { try { _renderHoardCounts(); } catch (_) {} });
                _scheduleHoardRender();
            }
            return;
        }
        if (type === "stats_snapshot" && Array.isArray(data.torrents)) {
            if (!_hoardAllTorrents || !_hoardAllTorrents.length) return;
            const byHash = new Map();
            for (let i = 0; i < _hoardAllTorrents.length; i++) {
                const row = _hoardAllTorrents[i];
                // Local-engine stats: an agent's copy of the same hash keeps its
                // own figures, which arrive with the next hydration.
                if (!_isLocalAgent(row.agent)) continue;
                byHash.set(row.info_hash, row);
            }
            let touched = 0;
            for (const m of data.torrents) {
                const t = byHash.get(m.info_hash);
                if (!t) continue;
                t.state = _STATUS_TO_STATE[m.status] || t.state;
                t.upload_rate = m.upload_rate;
                t.download_rate = m.download_rate;
                t.total_upload = m.total_uploaded;
                t.total_download = m.total_downloaded;
                t.num_peers = m.peers_connected;
                if (typeof m.progress === "number") t.progress = m.progress;
                if (t.total_size && t.total_size > 0 && t.total_done > 0) {
                    t.ratio = t.total_upload / t.total_done;
                }
                touched++;
            }
            if (touched > 0) _scheduleHoardRender();
        } else if (type === "torrent_removed" && data.info_hash) {
            if (_hoardAllTorrents) {
                _hoardAllTorrents = _hoardAllTorrents.filter(
                    t => !(t.info_hash === data.info_hash && _isLocalAgent(t.agent)));
                _scheduleHoardRender();
            }
        }
    };
    _sseConn.onerror = (_e) => {
        // EventSource auto-reconnects. Log only.
    };
}

let _hoardRenderScheduled = false;
function _scheduleHoardRender() {
    if (_hoardRenderScheduled) return;
    _hoardRenderScheduled = true;
    requestAnimationFrame(() => {
        _hoardRenderScheduled = false;
        if (document.hidden) return;
        try { renderHoardTable(); } catch (_) {}
    });
}

// Suspend SSE while tab is hidden, EventSource is not throttled by browser
// background policies, and rendering ~13k-torrent tables once a second under
// throttled GC was leaking GBs of allocations and eventually OOM-killing the
// renderer (blank tab on return, refresh wedged, only new tab recovers).
document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
        if (_sseConn) { _sseConn.close(); _sseConn = null; }
    } else {
        _syncCursor = 0; // force full re-hydration on tab-return (a delta
                         // reconnect leaves surviving rows stale: id-not-name,
                         // missing category, frozen stats).
        setupHoardSSE();
        _hoardLastFetch = 0;
        updateOverview();
        if (typeof updateHoardStats === "function") updateHoardStats();
    }
});

window.addEventListener("pagehide", () => { if (_sseConn) { _sseConn.close(); _sseConn = null; } });


// ─── Settings (default.toml editor) ─────────────────────
let _settingsOrig = {};
function _isObj(v) { return v && typeof v === "object" && !Array.isArray(v); }

// Echappement HTML global (le `const esc` historique est local a une autre fonction).
// ─── Incognito (display-only anonymization for screenshots / demos) ─────────
// Deterministic per info_hash / string so labels are stable across refreshes.
// Only the DISPLAY is masked; state keeps real values, so filters/actions work.
let _incognito = (typeof localStorage !== "undefined" && localStorage.getItem("hydra_incognito") === "1");
const _INCO_DISTROS = [
    "ubuntu-24.04.2-desktop-amd64", "debian-12.9.0-amd64-netinst", "fedora-41-workstation-x86_64",
    "archlinux-2025.01.01-x86_64", "linuxmint-22-cinnamon-64bit", "manjaro-kde-24.0-x86_64",
    "pop-os_22.04_amd64", "openSUSE-Leap-15.6-DVD-x86_64", "kali-linux-2024.4-installer-amd64",
    "AlmaLinux-9.5-x86_64-dvd", "Rocky-9.5-x86_64-minimal", "gentoo-install-amd64-2025",
    "void-live-x86_64-2024", "elementaryos-8.0-stable", "Zorin-OS-17-Core-64bit",
];
const _INCO_CATS = ["Ubuntu", "Debian", "Fedora", "Arch", "Mint", "Manjaro", "Pop!_OS", "openSUSE", "Kali", "Rocky"];
function _incoHash(s) {
    let h = 5381; s = String(s == null ? "" : s);
    for (let i = 0; i < s.length; i++) h = ((h << 5) + h + s.charCodeAt(i)) >>> 0;
    return h;
}
function incoName(t) {
    const real = (t && (t.name || (t.info_hash ? t.info_hash.substring(0, 16) : ""))) || "";
    if (!_incognito) return real;
    const seed = (t && t.info_hash) || real;
    return _INCO_DISTROS[_incoHash(seed) % _INCO_DISTROS.length] + ".iso";
}
function incoCat(c) {
    if (!_incognito) return c || "\u2014";
    if (!c) return "\u2014";
    return _INCO_CATS[_incoHash(c) % _INCO_CATS.length];
}
function incoIP(ip) {
    if (!_incognito || !ip) return ip;
    // TEST-NET-3 (203.0.113.0/24), reserved for docs/examples, clearly fake.
    return "203.0.113." + (_incoHash(ip) % 254 + 1);
}
function incoPath(p) {
    // Save paths can leak real folders/usernames, replace with a plausible
    // but fake path, distinct per real path.
    if (!_incognito || !p) return p;
    return "/downloads/" + _INCO_CATS[_incoHash(p) % _INCO_CATS.length].toLowerCase().replace(/[^a-z0-9]/g, "");
}
function incoExitIP(ip) {
    // Own egress identity: mask with an obviously-redacted value so nobody
    // mistakes it for a real IP on a screenshot.
    if (!_incognito || !ip) return ip;
    return "\u2022\u2022\u2022.\u2022\u2022\u2022.\u2022\u2022\u2022.\u2022\u2022\u2022";
}
function toggleIncognito() {
    _incognito = !_incognito;
    try { localStorage.setItem("hydra_incognito", _incognito ? "1" : "0"); } catch (e) {}
    location.reload();
}
window.addEventListener("load", function () {
    const b = document.getElementById("incognito-toggle");
    if (b) {
        b.classList.toggle("active", _incognito);
        b.style.color = _incognito ? "var(--accent-purple)" : "#ffffff";
        b.title = (_incognito ? t("Incognito ON") + ". " : "") + t("Anonymize names, categories & IPs for screenshots");
    }
});

function esc(s) {
    return String(s).replace(/[&<>"']/g, ch => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[ch]));
}
// Regroupement des sections toml par domaine (ordre = ordre d'affichage).
// `tops` = sections toml de 1er niveau ; les sous-tables (race.custom_choking...) heritent du top.
// Keys the Network tab owns. Shown there, hidden from the flat lists, because
// the Network tab writes them as a set: editing one of them on its own can
// produce a combination the tab would have refused.
const NET_OWNED_KEYS = new Set([
    "listen_port", "listen_interfaces", "bind_interface", "enable_ipv6",
    "listen_port_proxy_v2", "listen_addr_proxy_v2", "proxy_v2_trusted_sources",
    "socks5_outbound_host", "socks5_outbound_port", "socks5_outbound_user",
    "socks5_outbound_pass", "announce_proxy", "announce_ip",
]);

const SETTINGS_DOMAINS = [
    { id: "daemon",   label: "General",              icon: "\u2699\uFE0F", tops: ["daemon"] },
    { id: "race",     label: "Session Race", icon: "\u{1F3C1}",     tops: ["race", "race_drain"] },
    { id: "hoard",    label: "Session Hoard",          icon: "\u{1F4E6}",     tops: ["hoard"] },
    { id: "trackers", label: "Trackers & Network",             icon: "\u{1F310}",     tops: ["announce_passkeys", "announce_clients", "announce_ip_modes"] },
    { id: "observ",   label: "Observability",                 icon: "\u{1F4CA}",     tops: ["metrics", "peer_intel"] },
    { id: "maint",    label: "Maintenance",                   icon: "\u{1F9F9}",     tops: ["vpn_speedtest"] },
];
const _SETTINGS_DESC = {
    // [daemon]
    api_host: "IP the HTTP API/WebUI binds to (0.0.0.0 = all interfaces).",
    api_port: "TCP port for the HTTP API and WebUI.",
    api_key: "Secret required in the X-API-Key header to call the API.",
    agent_token: "Shared bearer token a front must present to drive this node's agent (gRPC) data-plane. Empty = no auth, which is only safe on a private LAN. $HYDRA_AGENT_TOKEN overrides this, and --agent-token overrides both.",
    data_dir: "Root directory for daemon state (categories, DBs, configs).",
    create_torrent_folder: "Create a per-torrent subfolder for single-file torrents (like qBittorrent's subfolder option). Off = single files saved directly in the category folder; multi-file torrents always keep their own folder. Applies to newly added torrents only.",
    // session (race/hoard)
    listen_port: "TCP/UDP port this session listens on for incoming peers.",
    listen_interfaces: "Comma-separated ip:port bind list (multi-homing).",
    enable_ipv6: "Also listen for peers over IPv6, and accept the IPv6 peers trackers and PEX offer. Off = IPv4 only. Only enable it if this host has working IPv6, otherwise you announce an address nobody can reach.",
    listen_port_proxy_v2: "Extra listener expecting HAProxy PROXY-protocol v2 (real peer IP). 0 = off.",
    listen_addr_proxy_v2: "Explicit bind address for the PROXY-v2 listener. Empty = [::] wildcard.",
    proxy_v2_trusted_sources: "Source IPs allowed to send PROXY-v2 headers.",
    socks5_outbound_host: "SOCKS5 proxy host for this session's outbound peer connections.",
    socks5_outbound_port: "SOCKS5 proxy port for outbound peer connections.",
    socks5_outbound_user: "SOCKS5 username (if authenticated).",
    socks5_outbound_pass: "SOCKS5 password (if authenticated).",
    announce_proxy: "SOCKS5 proxy for this session's TRACKER ANNOUNCES (socks5h://user:pass@host:port). Separate from socks5_outbound_*, which only covers peer connections: without this, announces leave directly and the tracker records this host's own address. UDP trackers are skipped while it is set.",
    announce_ip: "Address advertised in the BEP-7 ip= announce parameter. Empty = omit it and let the tracker observe the source address (correct for almost every setup).",
    max_connections: "Global cap on simultaneous peer connections for this session.",
    max_uploads_per_torrent: "Max simultaneous upload slots per torrent (-1 = unlimited).",
    peer_timeout: "Seconds of inactivity before disconnecting a peer.",
    inactivity_timeout: "Seconds before an idle peer is considered inactive.",
    active_seeds: "Max torrents actively seeding (-1 = unlimited).",
    active_limit: "Max active torrents overall (-1 = unlimited).",
    active_downloads: "Max torrents actively downloading at once.",
    file_pool_size: "Max file handles kept open by the disk cache.",
    upload_rate_limit: "Upload speed cap in bytes/s (0 = unlimited).",
    announce_rate_limit: "Cap on outbound tracker announces for this session, in announces per second. 0 = unlimited. Use it when a VPN or a firewall drops the burst a large library sends at once: the same announces then go out spread over time. Fractional values allowed (0.5 = one announce every 2 seconds).",
    // [race.custom_choking]
    tick_interval_seconds: "How often the custom choker re-evaluates peers (seconds).",
    strategy: "Custom choking strategy name.",
    max_unchoked: "Max peers unchoked by the custom choker.",
    rarity_weight: "Weight given to piece rarity when ranking peers (0-1).",
    speed_weight: "Weight given to peer speed when ranking peers (0-1).",
    // [arr_cleanup]
    radarr_url: "Radarr base URL for the cleanup integration.",
    radarr_api_key: "Radarr API key.",
    sonarr_url: "Sonarr base URL.",
    sonarr_api_key: "Sonarr API key.",
    min_score: "Minimum custom-format score to keep a release.",
    // [vpn_speedtest]
    iperf3_server: "iperf3 server used to measure peer-facing bandwidth.",
    iperf3_port: "iperf3 server port (retries base..+8 if busy).",
    interval_secs: "Seconds between speedtest runs.",
    duration_secs: "Duration of each iperf3 run (seconds).",
    // [proxy]
    socks5_host: "SOCKS5 host used by the orchestrator (public IP + speedtest).",
    socks5_port: "SOCKS5 port.",
    socks5_user: "SOCKS5 username (if authenticated).",
    socks5_pass: "SOCKS5 password (if authenticated).",
    // [race_drain]
    check_interval_seconds: "How often disk usage is checked (seconds).",
    high_watermark_pct: "Disk-usage % that triggers purging old race data.",
    low_watermark_pct: "Purge stops once disk usage drops to this %.",
    race_path: "Filesystem path monitored/purged for race data.",
    min_age_minutes: "Minimum age before a race torrent can be purged (minutes).",
    // [notify]
    webhook_url: "Discord webhook URL for notifications.",
    // generique
    enabled: "Enable this feature.",
};
const _SETTINGS_COMMON = new Set([
    // WebUI / general (mirrors qBittorrent's everyday Options)
    "daemon::api_host", "daemon::api_port", "daemon::api_key", "daemon::create_torrent_folder",
    "auth::username", "auth::password_hash",
    // Connection + Speed + Queueing, per engine (race & hoard)
    "race::listen_port", "race::bind_interface", "race::enable_ipv6", "race::max_connections", "race::max_uploads_per_torrent",
    "race::upload_rate_limit", "race::active_downloads", "race::active_seeds", "race::active_limit",
    "hoard::listen_port", "hoard::bind_interface", "hoard::enable_ipv6", "hoard::max_connections", "hoard::max_uploads_per_torrent",
    "hoard::upload_rate_limit", "hoard::active_downloads", "hoard::active_seeds", "hoard::active_limit",
    // Common toggles
    "vpn_speedtest::enabled", "race_drain::enabled",
]);
const _SETTINGS_FALLBACK = { id: "other", label: "Other", icon: "\u{1F4C1}", tops: [] };

function _domainForTop(top) {
    for (const d of SETTINGS_DOMAINS) if (d.tops.includes(top)) return d;
    return _SETTINGS_FALLBACK;
}

// Aplatit un objet de config en liste {path, scalars:[[k,v]...]} (une entree par table).
function _collectSections(obj, path, out) {
    const scalars = [], subs = [];
    for (const [k, v] of Object.entries(obj)) {
        if (_isObj(v)) subs.push([k, v]); else scalars.push([k, v]);
    }
    if (scalars.length) out.push({ path, scalars });
    for (const [k, v] of subs) _collectSections(v, path ? path + "." + k : k, out);
}

const _SETTINGS_DEFAULT = {
    "daemon::api_host": "0.0.0.0", "daemon::api_port": 8199,
    "daemon::data_dir": "/config",
    "daemon::create_torrent_folder": true,
    "race::listen_port": 16171, "race::max_connections": 4000,
    "race::max_uploads_per_torrent": 100,
    "race::peer_timeout": 30,
    "race::inactivity_timeout": 20,
    "race::active_seeds": 50, "race::active_limit": 100, "race::active_downloads": 20,
    "race::file_pool_size": 500,
    "race.custom_choking::enabled": true, "race.custom_choking::tick_interval_seconds": 2,
    "race.custom_choking::strategy": "rarity_captive", "race.custom_choking::max_unchoked": 30,
    "race.custom_choking::rarity_weight": 0.7, "race.custom_choking::speed_weight": 0.3,
    "hoard::listen_port": 16172, "hoard::max_connections": 8000,
    "hoard::max_uploads_per_torrent": 20,
    "hoard::peer_timeout": 90,
    "hoard::inactivity_timeout": 90,
    "hoard::active_seeds": -1, "hoard::active_limit": -1, "hoard::active_downloads": -1,
    "hoard::file_pool_size": 5000,
    "arr_cleanup::min_score": 0.6,
    "race_drain::enabled": true, "race_drain::check_interval_seconds": 60,
    "race_drain::high_watermark_pct": 95, "race_drain::low_watermark_pct": 85,
    "race_drain::race_path": "/race", "race_drain::min_age_minutes": 10,
};
const _SETTINGS_ENUM = {
    strategy: ["rarity_captive"],
};

function genApiKey(id) {
    const el = document.getElementById(id);
    if (!el) return;
    const b = new Uint8Array(24);
    crypto.getRandomValues(b);
    el.value = Array.from(b, x => x.toString(16).padStart(2, "0")).join("");
}
async function copyField(id, btn) {
    const el = document.getElementById(id);
    if (!el) return;
    try {
        await navigator.clipboard.writeText(el.value);
    } catch (e) {
        el.select();
        try { document.execCommand("copy"); } catch (e2) {}
    }
    if (btn) {
        const t = btn.textContent;
        btn.textContent = "";
        setTimeout(() => { btn.textContent = t; }, 1200);
    }
}

let _detectedIfaces = [];

function _settingField(id, v, k) {
    if (Array.isArray(v)) {
        // Non-scalaire : lecture seule. Un champ texte editable reecrirait le
        // tableau en string et casserait le type au boot (cf B1 part 3).
        return `<code class="sr-readonly" title="Array \u2014 edit in default.toml">${esc(JSON.stringify(v))}</code>`;
    }
    if (k === "bind_interface") {
        const cur = String(v || "");
        const names = (_detectedIfaces || []).map((i) => i.name);
        if (cur && !names.includes(cur)) names.unshift(cur);
        const optNone = `<option value=""${cur === "" ? " selected" : ""}>\u2014 none (all interfaces) \u2014</option>`;
        const o = [optNone].concat(names.map((n) => {
            const nic = (_detectedIfaces || []).find((i) => i.name === n);
            const lbl = nic ? `${n} \u2014 ${nic.ip}` : n;
            return `<option value="${esc(n)}"${n === cur ? " selected" : ""}>${esc(lbl)}</option>`;
        })).join("");
        return `<select id="${id}" class="sr-input">${o}</select>`;
    }
    const opts = _SETTINGS_ENUM[k];
    if (opts) {
        const cur = String(v);
        const vals = opts.includes(cur) ? opts : [cur, ...opts];
        const o = vals.map(x => `<option value="${esc(x)}"${x === cur ? " selected" : ""}>${esc(x)}</option>`).join("");
        return `<select id="${id}" class="sr-input">${o}</select>`;
    }
    if (k === "api_key") {
        return `<div class="sr-keygen">` +
            `<input type="text" id="${id}" value="${esc(String(v))}" class="sr-input">` +
            `<button type="button" class="btn-small" onclick="genApiKey('${id}')" title="Generate random key">\u21bb</button>` +
            `<button type="button" class="btn-small" onclick="copyField('${id}', this)" title="Copy">\u29c9</button>` +
            `</div>`;
    }
    if (typeof v === "boolean") {
        return `<label class="toggle"><input type="checkbox" id="${id}" ${v ? "checked" : ""}><span class="toggle-track"></span></label>`;
    }
    if (typeof v === "number") {
        return `<input type="number" id="${id}" value="${v}" step="any" class="sr-input sr-input-num">`;
    }
    return `<input type="text" id="${id}" value="${esc(String(v))}" class="sr-input">`;
}

async function changePassword() {
    const el = document.getElementById("newpass-input");
    const res = document.getElementById("newpass-result");
    const pw = el.value;
    if (pw.length < 6) {
        res.className = "result-msg error";
        res.textContent = t("Password too short (min 6).");
        return;
    }
    try {
        await api("/api/auth/password", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ password: pw }),
        });
        res.className = "result-msg success";
        res.textContent = t("Password changed.");
        el.value = "";
    } catch (e) {
        res.className = "result-msg error";
        res.textContent = t("Error: {msg}", { msg: e.message });
    }
}

async function updateSettings() {
    const editor = document.getElementById("settings-editor");
    if (!editor) return;
    try {
        const cfg = await api("/api/settings");
        _settingsOrig = {};
        try { _detectedIfaces = (await api("/api/network/interfaces")).interfaces || []; } catch (e) { _detectedIfaces = []; }

        // Bucketise les sections par domaine.
        const buckets = {};
        for (const [sec, v] of Object.entries(cfg)) {
            if (!_isObj(v)) continue;
            const dom = _domainForTop(sec);
            const out = [];
            _collectSections(v, sec, out);
            (buckets[dom.id] = buckets[dom.id] || []).push(...out);
        }

        const order = SETTINGS_DOMAINS.concat([_SETTINGS_FALLBACK]);
        let html = _languageCardHTML() + _unitsCardHTML() + `<div class="settings-toolbar">
            <input type="text" id="settings-search" class="settings-search" placeholder="${t("Search a setting\u2026")}" oninput="filterSettings()">
            <label class="settings-adv-toggle"><span class="toggle"><input type="checkbox" id="settings-show-adv" onchange="filterSettings()"><span class="toggle-track"></span></span> ${t("Show advanced settings")}</label>
            <span id="settings-search-count" class="settings-search-count"></span>
            <button class="btn-small btn-danger" style="margin-left:auto" onclick="resetSettings()">${t("Reset to defaults")}</button>
        </div>`;

        const _activeDomSaved = localStorage.getItem("hydra_settings_tab");
        let _tabsHtml = "";
        let _panelsHtml = "";
        // First tab on purpose: the connectivity keys are the ones a newcomer
        // has to get right, and the flat list below cannot show that they are
        // alternatives rather than twenty independent knobs.
        let _firstDom = "__network";
        _tabsHtml += `<button type="button" class="settings-tab" data-domain="__network" onclick="showSettingsPanel('__network')"><span class="sg-title">${t("Network")}</span></button>`;
        _panelsHtml += netModePanelHTML();
        for (const dom of order) {
            const sections = buckets[dom.id];
            if (!sections || !sections.length) continue;
            if (!_firstDom) _firstDom = dom.id;
            let count = 0;
            let body = "";
            for (const { path, scalars } of sections) {
                let rows = "";
                for (const [k, v] of scalars) {
                    if (NET_OWNED_KEYS.has(k) && (path === "race" || path === "hoard")) continue;
                    const id = "set__" + path + "__" + k;
                    if (!Array.isArray(v)) _settingsOrig[id] = { section: path, key: k, value: v };
                    const search = (path + " " + k).toLowerCase();
                    const _adv = _SETTINGS_COMMON.has(path + "::" + k) ? "" : ' data-adv="1"';
                    const _def = _SETTINGS_DEFAULT[path + "::" + k];
                    const _dtxt = t(_SETTINGS_DESC[k] || "");
                    const _descHtml = (_dtxt || _def !== undefined)
                        ? `<span class="sr-desc">${esc(_dtxt)}${_def !== undefined ? ` <span class="sr-default">${t("default:")} ${esc(String(_def))}</span>` : ""}</span>`
                        : "";
                    rows += `<div class="settings-row" data-search="${esc(search)}"${_adv}>
                        <div class="sr-label"><span class="sr-key">${esc(k)}</span>${_descHtml}<span class="sr-path">${esc(path)}</span></div>
                        <div class="sr-field">${_settingField(id, v, k)}</div>
                    </div>`;
                    count++;
                }
                if (!rows) continue;
                body += `<div class="settings-section"><div class="settings-section-title">[${esc(path)}]</div>${rows}</div>`;
            }
            if (dom.id === "race" || dom.id === "hoard") {
                body = `<p class="sr-desc" style="margin:0 0 12px">${t("Ports, interface and proxy live in the Network tab, which writes them as one coherent set.")}</p>` + body;
            }
            _tabsHtml += `<button type="button" class="settings-tab" data-domain="${dom.id}" onclick="showSettingsPanel('${dom.id}')"><span class="sg-title">${esc(t(dom.label))}</span> <span class="sg-count">${count}</span></button>`;
            _panelsHtml += `<div class="settings-panel" data-domain="${dom.id}"><div class="settings-group-body">${body}</div>` +
                `<div class="settings-save-bar"><button class="btn-primary" onclick="saveSettings()">${t("Save settings")}</button></div></div>`;
        }
        _tabsHtml += `<button type="button" class="settings-tab" data-domain="__import" onclick="showSettingsPanel('__import')"><span class="sg-title">${t("Import")}</span></button>`;
        _panelsHtml += `<div class="settings-panel" data-domain="__import"><div class="settings-import"><h3 style="margin:0 0 .5em">${t("Import from another client")}</h3><p class="sr-desc" style="margin:.4em 0 1em">${t("From <b>qBittorrent</b> (via its WebUI) or <b>Transmission</b> (by reading its config folder \u2014 it does not even need to be running). Seeds data already on disk (completed torrents skip the hash-check) \u2014 nothing is re-downloaded. Torrents stopped in the old client stay stopped. Already-present torrents are skipped, so it is safe to re-run.")}</p><button class="btn-primary" onclick="importFromQbit()">${t("Import\u2026")}</button></div></div>`;
        html += `<div class="settings-tabs" id="settings-tabs">${_tabsHtml}</div><div class="settings-panels" id="settings-panels">${_panelsHtml}</div>`;
        editor.innerHTML = html;
        {
            const _active = (_activeDomSaved === "__network" || (_activeDomSaved && buckets[_activeDomSaved] && buckets[_activeDomSaved].length)) ? _activeDomSaved : _firstDom;
            if (_active) showSettingsPanel(_active);
        }
        netModeInit();
        filterSettings();
        // Re-rendering the page must not swallow a restart that is still owed.
        _applyRestartBanner();
    } catch (e) {
        editor.textContent = t("Error: {msg}", { msg: e.message });
    }
}

// Filtre live : masque les lignes/sections/groupes sans match, ouvre les groupes qui matchent.
// A restart owed to the user outlives a re-render and a tab switch: the config
// is already written, so forgetting to say it needs a restart leaves the daemon
// running settings nobody can see any more.
let _restartPending = null;

function _setRestartBanner(html, cls) {
    _restartPending = html ? { html: html, cls: cls || "result-msg success" } : null;
    _applyRestartBanner();
}

function _applyRestartBanner() {
    const banner = document.getElementById("settings-restart-banner");
    if (!banner) return;
    if (!_restartPending) { banner.style.display = "none"; return; }
    banner.className = _restartPending.cls;
    banner.innerHTML = _restartPending.html;
    banner.style.display = "block";
}

function showSettingsPanel(id) {
    document.querySelectorAll("#settings-panels .settings-panel").forEach((pn) => { pn.hidden = pn.dataset.domain !== id; });
    document.querySelectorAll("#settings-tabs .settings-tab").forEach((t) => { t.classList.toggle("active", t.dataset.domain === id); });
    localStorage.setItem("hydra_settings_tab", id);
}

window.addEventListener("beforeunload", (e) => {
    const tab = document.querySelector(".tab.active");
    if (tab && tab.dataset.tab === "config" && _settingsDirtyCount() > 0) {
        e.preventDefault();
        e.returnValue = "";
    }
});

function filterSettings() {
    const box = document.getElementById("settings-search");
    if (!box) return;
    const q = box.value.trim().toLowerCase();
    const advBox = document.getElementById("settings-show-adv");
    const showAdv = !!(advBox && advBox.checked);
    const searching = !!q;
    const active = localStorage.getItem("hydra_settings_tab");
    const tabs = document.getElementById("settings-tabs");
    if (tabs) tabs.style.display = searching ? "none" : "";
    let shown = 0;
    document.querySelectorAll(".settings-panel").forEach((panel) => {
        let pVisible = 0;
        panel.querySelectorAll(".settings-section").forEach((sec) => {
            let secVisible = 0;
            sec.querySelectorAll(".settings-row").forEach((r) => {
                const advOk = showAdv || !r.dataset.adv;
                const m = advOk && (!q || r.dataset.search.includes(q));
                r.style.display = m ? "" : "none";
                if (m) { secVisible++; shown++; }
            });
            sec.style.display = secVisible ? "" : "none";
            pVisible += secVisible;
        });
        // Search mode: reveal every panel that has a match. Tab mode: only the
        // active panel is shown (the tab bar drives which one).
        panel.hidden = searching ? (pVisible === 0) : (panel.dataset.domain !== active);
    });
    const c = document.getElementById("settings-search-count");
    if (c) c.textContent = shown + (showAdv ? " settings" : " common");
}

function _readSettingField(id, orig) {
    const el = document.getElementById(id);
    if (!el) return orig.value;
    if (typeof orig.value === "boolean") return el.checked;
    if (typeof orig.value === "number") return el.value === "" ? orig.value : Number(el.value);
    return el.value;
}

// Which restart a setting needs:
//   hot    = applied live, no restart (currently: listen_port)
//   engine = the torrent engines must restart (all [race]/[hoard] knobs)
//   full   = the whole daemon restarts ([daemon], [auth], services)
function _settingTier(section, key) {
    if (section === "daemon" || section === "auth") return "full";
    if (section === "race" || section === "hoard" || section.startsWith("race.") || section.startsWith("hoard.")) {
        if (key === "listen_port") return "hot";
        return "engine";
    }
    return "full";
}

async function saveSettings() {
    const banner = document.getElementById("settings-restart-banner");
    const changes = [];
    for (const [id, orig] of Object.entries(_settingsOrig)) {
        const nv = _readSettingField(id, orig);
        if (nv !== orig.value) changes.push({ section: orig.section, key: orig.key, value: nv });
    }
    banner.style.display = "block";
    banner.scrollIntoView({ block: "nearest", behavior: "smooth" });
    if (!changes.length) {
        banner.className = "result-msg info";
        banner.textContent = t("No changes.");
        return;
    }
    try {
        const r = await api("/api/settings", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ changes }),
        });
        for (const ch of changes) {
            const id = "set__" + ch.section + "__" + ch.key;
            if (_settingsOrig[id]) _settingsOrig[id].value = ch.value;
        }
        // If THIS browser's API key changed, update its localStorage so we don't
        // lock ourselves out (the WebUI reads its key from there).
        const _kc = changes.find(c => c.section === "daemon" && c.key === "api_key");
        if (_kc) { try { localStorage.setItem("hydra_api_key", String(_kc.value)); } catch (e) {} }

        // Tier the change set and do the minimum: apply hot keys live, only
        // restart when an engine/daemon setting actually changed.
        let tier = "hot";
        for (const ch of changes) {
            const t = _settingTier(ch.section, ch.key);
            if (t === "full") tier = "full";
            else if (t === "engine" && tier !== "full") tier = "engine";
        }
        const hotApplied = [];
        for (const ch of changes) {
            if (_settingTier(ch.section, ch.key) === "hot" && ch.key === "listen_port") {
                try {
                    await api(`/api/${ch.section}/listen-port`, {
                        method: "POST", headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ port: Number(ch.value) }),
                    });
                    hotApplied.push(t("{engine} listen port", { engine: ch.section }));
                } catch (e) { if (tier === "hot") tier = "engine"; } // fall back to restart
            }
        }
        const base = t("{n} setting(s) written to default.toml.", { n: r.changed });
        banner.className = "result-msg success";
        if (tier === "hot") {
            banner.innerHTML = base + " " + t("Applied live, no restart needed") +
                (hotApplied.length ? ` (${hotApplied.join(", ")})` : "") + ".";
        } else {
            const what = (tier === "full")
                ? t("Daemon/auth settings changed, a full restart is required.")
                : t("Engine settings changed, restart the torrent engines to apply.");
            banner.innerHTML = `${base} ${what} ` +
                `<button class="btn-small btn-danger" onclick="restartDaemon()" style="margin-left:8px">${t("Apply &amp; restart")}</button>`;
            if (_kc) banner.innerHTML += ' <span style="color:var(--text-secondary)">' + t("(API key updated for this browser)") + '</span>';
        }
    } catch (e) {
        banner.className = "result-msg error";
        banner.textContent = t("Error: {msg}", { msg: e.message });
    }
}

// Says exactly what survives and what does not, because the three values kept
// are the ones whose loss cannot be undone from the UI, and everything else
// really does go.
async function resetSettings() {
    const ov = document.createElement("div");
    ov.className = "modal-overlay";
    ov.innerHTML = `<div class="modal-box">
        <h3>${t("Reset to defaults")}</h3>
        <p class="modal-desc">${esc(t("Every setting goes back to what a fresh install ships: ports, network mode, tracker passkeys, client spoofs, engine tuning."))}</p>
        <p class="modal-desc">${esc(t("Your login, your API key and your data directory are kept, so you do not lock yourself out. A copy of the current config is saved next to it first."))}</p>
        <div class="modal-actions">
            <button class="btn-small" id="reset-cancel">${t("Cancel")}</button>
            <button class="btn-small btn-danger" id="reset-go">${t("Reset and restart")}</button>
        </div>
    </div>`;
    document.body.appendChild(ov);
    ov.querySelector("#reset-cancel").onclick = () => ov.remove();
    ov.querySelector("#reset-go").onclick = async () => {
        ov.remove();
        try {
            const r = await api("/api/settings/reset", { method: "POST" });
            _settingsOrig = {};
            _netOrig = null;
            _setRestartBanner(esc(t("Settings reset. Backup saved as {path}", { path: r.backup })));
            await restartDaemon(true);
        } catch (e) {
            _setRestartBanner(esc(t("Error: {msg}", { msg: e.message })), "result-msg error");
        }
    };
}

async function restartDaemon(skipConfirm) {
    if (!skipConfirm && !await hydraConfirm(t("Restart the Hydra daemon now? (running torrents resume on boot)"))) return;
    const banner = document.getElementById("settings-restart-banner");
    try {
        await api("/api/settings/restart", { method: "POST" });
        _setRestartBanner(esc(t("Restarting… the page will reload in a few seconds.")), "result-msg info");
        setTimeout(() => location.reload(), 7000);
    } catch (e) {
        banner.className = "result-msg error";
        banner.textContent = t("Error: {msg}", { msg: e.message });
    }
}

if (!API_KEY) promptLogin(t("Sign in to Hydra."));
if (API_KEY) maybeOfferImport();


// ─── Agents ─────────────────────────────────────────
let _editingAgent = null;
// One table, because there is one thing.
//
// This page used to show an Agents table and an "Engines on this machine"
// table, which listed the SAME rows twice with different columns -- and the two
// disagreed on what could be done to them: an agent could not be deleted, the
// engine behind it could. Since one agent is one engine, that split described a
// distinction that no longer exists. The row carries the engine's role and
// port, and its delete button removes the thing itself.
async function updateAgents() {
    updateRemovedAgents();
    try {
        // The ports are the reason for the extra calls: /api/agents names the
        // engines, /api/engines carries the live port of the ones started here,
        // and port-forward carries the primaries' -- which move on their own in
        // gluetun mode, so the config would be the wrong place to read them.
        const results = await Promise.all([
            api("/api/agents"),
            api("/api/engines").catch(function () { return []; }),
            api("/api/port-forward").catch(function () { return null; }),
        ]);
        const agents = results[0] || [];
        const extras = results[1] || [];
        const pf = results[2];
        const tbody = document.getElementById("agents-tbody");
        if (!agents.length) {
            tbody.innerHTML = '<tr><td colspan="6" class="empty">' + t("No agents") + '</td></tr>';
            return;
        }
        const extraById = {};
        extras.forEach(function (e) { extraById[e.id] = e; });

        tbody.innerHTML = agents.map(a => {
            const dot = a.online
                ? '<span class="mode-tag mode-hoard">' + t("online") + '</span>'
                : '<span class="mode-tag mode-race">' + t("offline") + '</span>';
            const local = a.kind === "local";
            const engines = a.engines || [];
            const deletable = local && engines.length === 1 && !!extraById[engines[0].id];
            let actions;
            if (deletable) {
                actions = `<button class="btn-small btn-danger" onclick="deleteEngine('${esc(engines[0].id)}')">${t("Delete")}</button>`;
            } else if (local) {
                // The two engines this daemon is built around. Removing one is
                // not an Agents-page action: it is a different daemon.
                actions = '<span class="sr-desc">' + t("built-in") + '</span>';
            } else {
                actions = `<button class="btn-small" onclick="editAgent('${esc(a.name)}','${esc(a.addr || "")}')">${t("Edit")}</button> <button class="btn-small btn-danger" onclick="deleteAgent('${esc(a.name)}')">${t("Delete")}</button>`;
            }
            const engineCell = engines.map(function (e) {
                let port = 0;
                if (extraById[e.id]) port = extraById[e.id].listen_port;
                else if (pf && local) port = e.role === "race" ? pf.race_port : (e.role === "hoard" ? pf.hoard_port : 0);
                const iface = extraById[e.id] && extraById[e.id].bind_interface;
                const bits = [esc(e.role || "")];
                if (port) bits.push('<span class="mono">' + port + "</span>");
                if (iface) bits.push('<span class="mono">' + esc(iface) + "</span>");
                return "<strong>" + esc(e.id) + "</strong> <span class=\"sr-desc\">" + bits.join(" &middot; ") + "</span>" + (e.online ? "" : " \u26a0");
            }).join("<br>") || '<span class="sr-desc">' + t("no engine") + "</span>";
            const where = local
                ? '<span class="sr-desc">' + t("this machine") + "</span>"
                : '<span class="mono" style="font-size:12px">' + esc(a.addr || "\u2014") + "</span>";
            const ifTip = (a.interfaces||[]).map(i=>i.name+": "+incoIP(i.ip)+(i.up?"":" " + t("(down)"))).join("\n");
            const exit = _exitIPMarkup(a.exit_ip, a.exit_ip_v6, !!a.ipv6_wanted);
            const exitTip = [exit.title, ifTip].filter(Boolean).join("\n");
            return `<tr><td><strong>${esc(a.name)}</strong></td><td>${engineCell}</td><td>${where}</td><td class="mono exit-ip-cell" style="font-size:12px" title="${esc(exitTip)}">${exit.html}</td><td>${dot}</td><td>${actions}</td></tr>`;
        }).join("");
    } catch (e) { console.error("Failed to update agents:", e); }
}
function showAgentForm(name = null, addr = "") {
    _editingAgent = name;
    const n = document.getElementById("ag-name");
    n.value = name || ""; n.disabled = !!name;
    document.getElementById("ag-addr").value = addr || "";
    document.getElementById("ag-token").value = "";
    document.getElementById("ag-tlsca").value = "";
    document.getElementById("ag-result").style.display = "none";
    // Editing only ever applies to a dialled node: a local engine's identity is
    // its id, and renaming it would orphan every placement naming it.
    const kind = document.getElementById("ag-kind");
    if (kind) { kind.value = "remote"; kind.disabled = !!name; }
    agentKindChanged();
    document.getElementById("agent-form").style.display = "block";
}
function hideAgentForm() { document.getElementById("agent-form").style.display = "none"; _editingAgent = null; }

// One form, two kinds of node. "This machine" starts an engine here; "another
// machine" registers one that is already running elsewhere. They were two
// separate screens -- an Agents form that could only ever be remote because it
// demanded an address, and an Add engine form that never said the word agent --
// which left no way to create a local agent at all, and no hint that an engine
// and an agent are now the same thing.
function agentKindChanged() {
    const local = document.getElementById("ag-kind").value === "local";
    document.querySelectorAll(".ag-local-only").forEach(function (el) { el.style.display = local ? "" : "none"; });
    document.querySelectorAll(".ag-remote-only").forEach(function (el) { el.style.display = local ? "none" : ""; });
    const n = document.getElementById("ag-name");
    n.placeholder = local ? "vpn7" : "seedbox-de";
    const hint = document.getElementById("ag-name-hint");
    if (hint) {
        // Say the resulting agent name outright: the engine id is what the user
        // types, but the name a category has to reference is the prefixed one.
        hint.textContent = local
            ? t("engine id — the agent will be called local-<id>")
            : t("how this node is referenced in a placement");
    }
    const testBtn = document.getElementById("ag-test-btn");
    if (testBtn) testBtn.style.display = local ? "none" : ""; // nothing to dial
}
function editAgent(name, addr) { showAgentForm(name, addr); }
function _agentPayload() {
    return {
        name: document.getElementById("ag-name").value.trim(),
        addr: document.getElementById("ag-addr").value.trim(),
        token: document.getElementById("ag-token").value.trim(),
        tls_ca: document.getElementById("ag-tlsca").value.trim(),
    };
}
function _agResult(msg, ok) {
    const r = document.getElementById("ag-result");
    r.textContent = msg;
    r.className = "result-msg " + (ok ? "success" : "error");
    r.style.display = "block";
}
async function testAgent() {
    const p = _agentPayload();
    if (!p.addr) { _agResult(t("Address required"), false); return; }
    try {
        const res = await api("/api/agents/test", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(p) });
        if (res.online) _agResult(t("✓ reachable"), true);
        else _agResult(t("✗ unreachable: {err}", { err: res.error || "" }), false);
    } catch (e) { _agResult(t("Error: {msg}", { msg: e.message }), false); }
}
async function saveAgent() {
    const kindEl = document.getElementById("ag-kind");
    if (kindEl && kindEl.value === "local" && !_editingAgent) {
        const id = document.getElementById("ag-name").value.trim();
        if (!id) { _agResult(t("Engine id required"), false); return; }
        const role = document.getElementById("ag-role").value;
        const port = parseInt(document.getElementById("ag-port").value) || 0;
        // Starting an engine takes a few seconds -- a Typhon process, a store
        // to open, torrents to reload -- so say what is happening instead of
        // leaving a dead-looking button, and refuse a second click meanwhile.
        const btn = document.getElementById("ag-save-btn");
        const btnLabel = btn ? btn.textContent : "";
        if (btn) { btn.disabled = true; btn.textContent = t("Starting..."); }
        _agResult(t("Starting the engine, this takes a few seconds..."), true);
        try {
            const res = await api("/api/engines", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id: id, role: role, listen_port: port }) });
            hideAgentForm();
            // No restart any more: the daemon starts the engine and registers
            // it as its own agent before answering. A node with no engine host
            // (front-only) still answers restart_required, so honour it.
            if (res && res.restart_required) {
                const banner = document.getElementById("restart-banner");
                if (banner) banner.style.display = "block";
            } else {
                hydraNotify(t("Engine {id} is running, as agent {agent}.", { id: id, agent: (res && res.agent) || ("local-" + id) }));
            }
            await updateAgents();
        } catch (e) { _agResult(t("Error: {msg}", { msg: e.message }), false); }
        finally { if (btn) { btn.disabled = false; btn.textContent = btnLabel; } }
        return;
    }
    const p = _agentPayload();
    if (!p.name || !p.addr) { _agResult(t("Name and address required"), false); return; }
    try {
        if (_editingAgent) {
            await api(`/api/agents/${encodeURIComponent(_editingAgent)}`, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(p) });
        } else {
            await api("/api/agents", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(p) });
        }
        hideAgentForm();
        await updateAgents();
    } catch (e) { _agResult(t("Error: {msg}", { msg: e.message }), false); }
}
async function deleteAgent(name) {
    if (!await hydraConfirm(t("Delete agent \"{name}\"?", { name: name }))) return;
    try { await api(`/api/agents/${encodeURIComponent(name)}`, { method: "DELETE" }); await updateAgents(); }
    catch (e) { hydraNotify(t("Error: {msg}", { msg: e.message })); }
}

// Soft-delete safety: an accidentally removed agent stays here for one-click
// restore (its config is parked server-side; the remote agent never stops).
async function updateRemovedAgents() {
    const box = document.getElementById("agents-removed");
    if (!box) return;
    try {
        const rm = await api("/api/agents/removed");
        if (!rm || !rm.length) { box.innerHTML = ""; return; }
        box.innerHTML = '<div class="sr-desc" style="margin-top:12px">' + t("Recently removed, restore in one click") + '</div>' +
            rm.map(a => `<div class="cat-agent-row"><span class="cat-agent-lbl"><strong>${esc(a.name)}</strong></span><span class="mono" style="font-size:12px">${esc(a.addr || "")}</span> <button class="btn-small" onclick="restoreAgent('${esc(a.name)}')">${t("Restore")}</button></div>`).join("");
    } catch (e) { box.innerHTML = ""; }
}
async function restoreAgent(name) {
    try { await api(`/api/agents/restore/${encodeURIComponent(name)}`, { method: "POST" }); await updateAgents(); }
    catch (e) { hydraNotify(t("Error: {msg}", { msg: e.message })); }
}


// ─── Trackers ───────────────────────────────────────────
const TRACKER_PRESETS = {
    qb522: { pid: "-qB5220-", ua: "qBittorrent/5.2.2" },
    qb461: { pid: "-qB4610-", ua: "qBittorrent/4.6.1" },
    tr405: { pid: "-TR4050-", ua: "Transmission/4.0.5" },
    de211: { pid: "-DE211s-", ua: "Deluge 2.1.1" },
};
function applyTrackerPreset() {
    const p = TRACKER_PRESETS[document.getElementById("trk-preset").value];
    if (!p) return;
    document.getElementById("trk-pid").value = p.pid;
    document.getElementById("trk-ua").value = p.ua;
}
// Whitelist-enforcing trackers reject the real client outright, so someone who
// wants to look like qBittorrent generally wants it on all of them. Applied one
// by one through the Edit form, that is where a tracker gets forgotten.
async function spoofAllTrackers(clear) {
    const out = document.getElementById("trackers-bulk-result");
    const spoof = { peer_id_prefix: "-qB5220-", user_agent: "qBittorrent/5.2.2" };
    const body = clear ? { peer_id_prefix: "", user_agent: "" } : spoof;
    const question = clear
        ? t("Remove the client spoof from every tracker?")
        : t("Make every tracker see this client as qBittorrent 5.2.2?");
    if (!await hydraConfirm(question)) return;
    if (out) out.textContent = t("Applying…");
    try {
        const r = await api("/api/announce/clients/bulk", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
        });
        if (out) {
            out.textContent = t("Applied to {n} tracker(s).", { n: r.applied });
            if (r.not_persisted && r.not_persisted.length) {
                out.textContent += " " + t("{n} could not be written to the config file and will be lost on restart.", { n: r.not_persisted.length });
            }
        }
        _trackersSig = "";
        updateTrackers();
    } catch (e) {
        if (out) out.textContent = t("Error: {msg}", { msg: e.message });
    }
}

async function updateTrackers() {
    try {
        const rows = await api("/api/trackers");
        const tbody = document.getElementById("trackers-tbody");
        if (!rows || !rows.length) {
            tbody.innerHTML = `<tr><td colspan="8" class="empty">${t("No tracker known yet. They appear here once a torrent announces, or as soon as you give one a setting.")}</td></tr>`;
            return;
        }
        const _thtml = rows.map(r => {
            const status = r.ok
                ? '<span class="mode-tag mode-hoard">ok</span>'
                : '<span class="mode-tag mode-race">error</span>';
            const spoof = r.spoofed
                ? `<span class="mode-tag mode-hoard">${esc(r.peer_id_prefix || "spoof")}</span>`
                : '<span class="sr-desc">-</span>';
            const passkey = r.passkey_set
                ? '<span class="mode-tag mode-hoard">set</span>'
                : '<span class="sr-desc">-</span>';
            const err = r.last_error ? esc(r.last_error) : "-";
            // Read-only here on purpose: this row carries a torrent count and a
            // last-announce time, so it is rewritten on every poll. A control
            // living in it would be torn out from under the pointer mid-click.
            // The family is set from the Edit form, like the spoof and passkey.
            const cur = r.ip_mode || "auto";
            const ipmode = cur !== "auto"
                ? `<span class="mode-tag mode-hoard">${esc(cur)}</span>`
                : '<span class="sr-desc">auto</span>';
            return `<tr><td><strong>${esc(r.host)}</strong></td><td>${r.torrents}</td><td>${status}</td><td>${spoof}</td><td>${passkey}</td><td>${ipmode}</td><td class="sr-desc" style="max-width:280px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis" title="${esc(r.last_error || "")}">${err}</td><td><button class="btn-small" onclick="editTracker('${esc(r.host)}','${esc(r.peer_id_prefix || "")}','${esc(r.user_agent || "")}','${esc(cur)}')">Edit</button></td></tr>`;
        }).join("");
        if (_thtml === _trackersSig) return;
        _trackersSig = _thtml;
        tbody.innerHTML = _thtml;
    } catch (e) { console.error("Failed to update trackers:", e); }
}
// --- Per-tracker bench stats (Trackers tab) ---
let _trkStatsChart = null;
let _trackersSig = "";
let _trkStatsSig = "";
let _trkSelSig = "";

async function loadTrackerStats() {
    try {
        const rows = await api("/api/benchmark/trackers/current");
        _renderTrackerStatsTable(rows || []);
        _populateTrackerStatsSelect(rows || []);
    } catch (e) { console.error("Failed to load tracker stats:", e); }
}

function _renderTrackerStatsTable(rows) {
    const tbody = document.getElementById("trkstats-tbody");
    if (!tbody) return;
    if (!rows.length) {
        tbody.innerHTML = '<tr><td colspan="9" class="empty">No tracker stats yet</td></tr>';
        return;
    }
    const byTracker = {};
    rows.forEach(r => { (byTracker[r.tracker] = byTracker[r.tracker] || []).push(r); });
    const trackers = Object.keys(byTracker).sort();
    let html = "";
    trackers.forEach(trk => {
        const engs = byTracker[trk].sort((a, b) => a.engine.localeCompare(b.engine));
        engs.forEach((r, i) => {
            const tag = r.engine === "hoard"
                ? '<span class="mode-tag mode-hoard">hoard</span>'
                : '<span class="mode-tag mode-race">race</span>';
            const ratio = r.cum_downloaded > 0 ? (r.cum_uploaded / r.cum_downloaded).toFixed(2) : "∞";
            html += `<tr>` +
                `<td>${i === 0 ? `<strong>${esc(trk)}</strong>` : ""}</td>` +
                `<td>${tag}</td>` +
                `<td>${formatSpeed(r.upload_rate)}</td>` +
                `<td>${formatSpeed(r.download_rate)}</td>` +
                `<td>${Math.round(r.peers)}</td>` +
                `<td>${Math.round(r.active)}/${Math.round(r.torrents)}</td>` +
                `<td>${formatBytes(r.cum_uploaded)}</td>` +
                `<td>${formatBytes(r.cum_downloaded)}</td>` +
                `<td>${ratio}</td>` +
                `</tr>`;
        });
    });
    if (html === _trkStatsSig) return;
    _trkStatsSig = html;
    tbody.innerHTML = html;
}

function _populateTrackerStatsSelect(rows) {
    const sel = document.getElementById("trkstats-select");
    if (!sel) return;
    const trackers = [...new Set(rows.map(r => r.tracker))].sort();
    const sig = trackers.join("|");
    const cur = sel.value;
    if (sig !== _trkSelSig) {
        _trkSelSig = sig;
        sel.innerHTML = trackers.map(t => `<option value="${esc(t)}">${esc(t)}</option>`).join("");
        if (trackers.includes(cur)) sel.value = cur;
    }
    if (sel.value && !_trkStatsChart) loadTrackerStatsChart(sel.value);
}

async function loadTrackerStatsChart(tracker) {
    if (!tracker) return;
    try {
        const end = Math.floor(Date.now() / 1000);
        const start = end - 24 * 3600;
        const rows = await api(`/api/benchmark/trackers/range?start=${start}&end=${end}&tracker=${encodeURIComponent(tracker)}`);
        _renderTrackerStatsChart(rows || []);
    } catch (e) { console.error("Failed to load tracker chart:", e); }
}

function _renderTrackerStatsChart(rows) {
    if (typeof Chart === "undefined") return;
    const tsSet = [...new Set(rows.map(r => r.ts))].sort((a, b) => a - b);
    const hoardMap = {}, raceMap = {};
    rows.forEach(r => { (r.engine === "hoard" ? hoardMap : raceMap)[r.ts] = r.upload_rate; });
    const labels = tsSet.map(t => new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }));
    const hoardSeries = tsSet.map(t => hoardMap[t] ?? null);
    const raceSeries = tsSet.map(t => raceMap[t] ?? null);
    if (!_trkStatsChart) {
        _trkStatsChart = _mkDualChart("trkstats-chart", t("hoard ↑"), "#2ea043", t("race ↑"), "#d29922", formatSpeed);
        _trkStatsChart.options.plugins.legend.display = true;
    }
    _trkStatsChart.data.labels = labels;
    _trkStatsChart.data.datasets[0].data = hoardSeries;
    _trkStatsChart.data.datasets[1].data = raceSeries;
    _trkStatsChart.update();
}

function showTrackerForm(host = "", pid = "", ua = "", ipmode = "auto") {
    document.getElementById("trk-host").value = host;
    document.getElementById("trk-preset").value = "";
    document.getElementById("trk-pid").value = pid;
    document.getElementById("trk-ua").value = ua;
    document.getElementById("trk-passkey").value = "";
    document.getElementById("trk-ipmode").value = ipmode || "auto";
    document.getElementById("trk-result").style.display = "none";
    document.getElementById("trk-form").style.display = "block";
}
function hideTrackerForm() { document.getElementById("trk-form").style.display = "none"; }
function editTracker(host, pid, ua, ipmode) { showTrackerForm(host, pid, ua, ipmode); }
function _trkResult(msg, ok) {
    const r = document.getElementById("trk-result");
    r.textContent = msg; r.className = "result-msg " + (ok ? "success" : "error"); r.style.display = "block";
}
function _trkHost() { return document.getElementById("trk-host").value.trim(); }
async function saveTracker() {
    const host = _trkHost();
    if (!host) { _trkResult(t("Host required"), false); return; }
    try {
        await api("/api/announce/clients", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, peer_id_prefix: document.getElementById("trk-pid").value.trim(), user_agent: document.getElementById("trk-ua").value.trim() }) });
        const pk = document.getElementById("trk-passkey").value.trim();
        if (pk) await api("/api/announce/passkeys", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, passkey: pk }) });
        await api("/api/announce/ip-modes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, mode: document.getElementById("trk-ipmode").value }) });
        _trackersSig = "";
        hideTrackerForm(); await updateTrackers();
    } catch (e) { _trkResult(t("Error: {msg}", { msg: e.message }), false); }
}
async function clearTrackerSpoof() {
    const host = _trkHost();
    if (!host) { _trkResult(t("Host required"), false); return; }
    try { await api("/api/announce/clients", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, peer_id_prefix: "" }) }); hideTrackerForm(); await updateTrackers(); }
    catch (e) { _trkResult(t("Error: {msg}", { msg: e.message }), false); }
}
async function clearTrackerPasskey() {
    const host = _trkHost();
    if (!host) { _trkResult(t("Host required"), false); return; }
    try { await api("/api/announce/passkeys", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, passkey: "" }) }); hideTrackerForm(); await updateTrackers(); }
    catch (e) { _trkResult(t("Error: {msg}", { msg: e.message }), false); }
}

// --- Local engines ---

// updateEngines and its table are gone: the Agents table above lists these
// engines, because each one IS an agent. Two tables for one thing is what made
// the page read as a distinction between agents and engines that the daemon
// stopped making in 3.138.0.

async function deleteEngine(id){
    // Named as the agent, because that is the row the button sits in and the
    // name every category placement refers to.
    if(!await hydraConfirm(t("Delete agent local-{id}? Its engine stops seeding right away.", { id: id }))) return;
    hydraNotify(t("Stopping engine {id}...", { id: id }));
    try{
        const res = await api("/api/engines/" + encodeURIComponent(id), {method:"DELETE"});
        // Only a node that could not stop it asks for a restart now.
        if(res && res.restart_required) document.getElementById("restart-banner").style.display="block";
        await updateAgents();
    }catch(err){ hydraNotify(t("Delete failed: {err}", { err: err })); }
}
async function restartHydra(){
    if(!await hydraConfirm(t("Restart Hydra to apply engine changes? (~40s)"))) return;
    try{ await api("/api/restart", {method:"POST"}); }catch(err){}
    hydraNotify(t("Restarting, reconnect in ~40s."));
}


// ── Resizable torrent table columns (drag the right edge; widths persist) ──
function initResizableColumns(table, key) {
    if (!table || table._colResizeInit) return;
    table._colResizeInit = true;
    const ths = Array.from(table.querySelectorAll("thead th[data-col]"));
    if (!ths.length) return;
    const goFixed = () => {
        if (table._colFixed) return;
        // Snapshot current widths while visible, then lock the layout so a drag
        // only moves the grabbed column instead of reflowing the whole table.
        ths.forEach(th => { th.style.width = th.offsetWidth + "px"; });
        table.style.tableLayout = "fixed";
        table._colFixed = true;
    };
    // Restore saved widths (works even while the tab is hidden: no measuring).
    const saved = JSON.parse(localStorage.getItem(key) || "{}");
    let anySaved = false;
    ths.forEach(th => {
        const w = saved[th.dataset.col];
        if (w) { th.style.width = w + "px"; anySaved = true; }
    });
    if (anySaved) { table.style.tableLayout = "fixed"; table._colFixed = true; }
    ths.forEach(th => {
        if (getComputedStyle(th).position === "static") th.style.position = "relative";
        const grip = document.createElement("div");
        grip.className = "col-grip";
        grip.addEventListener("click", e => e.stopPropagation()); // never trigger sort
        grip.addEventListener("mousedown", e => {
            e.preventDefault();
            e.stopPropagation();
            goFixed();
            const startX = e.pageX, startW = th.offsetWidth;
            const move = ev => { th.style.width = Math.max(40, startW + ev.pageX - startX) + "px"; };
            const up = () => {
                document.removeEventListener("mousemove", move);
                document.removeEventListener("mouseup", up);
                document.body.style.cursor = "";
                const s = JSON.parse(localStorage.getItem(key) || "{}");
                s[th.dataset.col] = th.offsetWidth;
                localStorage.setItem(key, JSON.stringify(s));
            };
            document.addEventListener("mousemove", move);
            document.addEventListener("mouseup", up);
            document.body.style.cursor = "col-resize";
        });
        th.appendChild(grip);
    });
}
function initTorrentColumnResizers() {
    initResizableColumns(document.getElementById("race-table"), "hydra_cols_race");
    initResizableColumns(document.getElementById("hoard-table"), "hydra_cols_hoard");
}
if (document.readyState !== "loading") initTorrentColumnResizers();
else document.addEventListener("DOMContentLoaded", initTorrentColumnResizers);


// ─── Piece availability map (restored: dropped by the 2026-07-10 overview
// refactor, which broke the torrent detail panel with a ReferenceError). ───
function renderPieceMap(piecesHave, piecesAvail, canvasId, infoId, cardId) {
    const card = document.getElementById(cardId || "detail-pieces-card");
    const canvas = document.getElementById(canvasId);
    const info = document.getElementById(infoId);

    if (!piecesHave || piecesHave.length === 0) {
        if (card) card.style.display = "none";
        return;
    }
    if (card) card.style.display = "";

    const total = piecesHave.length;
    const have = piecesHave.filter(p => p === 1).length;
    const missing = total - have;
    if (info) info.textContent = t("({have}/{total}, {missing} missing)", { have: have, total: total, missing: missing });

    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    const width = canvas.parentElement.clientWidth - 16;
    canvas.width = width;
    canvas.height = 40;
    ctx.clearRect(0, 0, width, 40);

    for (let x = 0; x < width; x++) {
        const startPiece = Math.floor(x * total / width);
        const endPiece = Math.floor((x + 1) * total / width);
        if (startPiece >= total) break;

        let colHave = 0;
        let colTotal = Math.max(endPiece - startPiece, 1);
        let maxAvail = 0;

        for (let p = startPiece; p < endPiece && p < total; p++) {
            if (piecesHave[p]) colHave++;
            if (piecesAvail && piecesAvail[p] !== undefined) {
                maxAvail = Math.max(maxAvail, piecesAvail[p]);
            }
        }

        const ratio = colHave / colTotal;

        if (ratio >= 1) {
            ctx.fillStyle = "#2ecc71";
        } else if (ratio > 0) {
            ctx.fillStyle = "#f39c12";
        } else {
            if (maxAvail === 0) {
                ctx.fillStyle = "#e74c3c";
            } else if (maxAvail <= 2) {
                ctx.fillStyle = "#e67e22";
            } else {
                ctx.fillStyle = "#555";
            }
        }

        ctx.fillRect(x, 0, 1, 40);
    }
}

// ─── Data-driven table columns: order (drag) + visibility + sort ────────────
// Each column: {id, label, sort (sort key or null), render(t)->"<td>..."}. The
// header and every row are rendered from this list in the user's saved order,
// so dragging a header reorders the whole column and hiding one drops it. Both
// are persisted per-table in localStorage (hydra_colcfg_<table>).
const TABLE_COLS = {
    "hoard-table": [
        { id: "name", label: "Name", sort: "name", render: t => `<td title="${t.torrent_error ? (t.torrent_error_msg || 'Torrent error') : (t.tracker_error ? (t.tracker_error_msg || 'Tracker error') : t.info_hash)}">${esc(incoName(t))}${t.tracker_error ? ' <span class="tracker-warn">!</span>' : ''}${t.torrent_error ? ' <span class="torrent-err-badge">ERR</span>' : ''}</td>` },
        { id: "total_size", label: "Size", sort: "total_size", render: t => `<td>${t.total_size ? formatBytes(t.total_size) : "-"}</td>` },
        { id: "state", label: "State", sort: "state", render: t => { const d = displayState(t); return `<td><span class="state-badge ${d.cls}" title="${d.title}">${d.label}</span></td>`; } },
        { id: "progress", label: "Progress", sort: "progress", render: t => { const pct = (t.progress * 100).toFixed(1); return `<td><div class="progress-bar"><div class="progress-fill" style="width:${pct}%"></div></div><div class="progress-text">${pct}%</div></td>`; } },
        { id: "swarm_seeds", label: "Seeds", sort: "swarm_seeds", render: t => `<td>${t.swarm_seeds ?? "-"}</td>` },
        { id: "swarm_leechers", label: "Leechers", sort: "swarm_leechers", render: t => `<td>${t.swarm_leechers ?? "-"}</td>` },
        { id: "download_rate", label: "Down", sort: "download_rate", render: t => `<td>${formatSpeed(t.download_rate ?? 0)}</td>` },
        { id: "upload_rate", label: "Up", sort: "upload_rate", render: t => `<td>${formatSpeed(t.upload_rate)}</td>` },
        { id: "ratio", label: "Ratio", sort: "ratio", render: t => `<td>${displayRatio(t).toFixed(2)}</td>` },
        { id: "tracker_host", label: "Tracker", sort: "tracker_host", render: t => `<td>${esc(t.tracker_host || "-")}</td>` },
        { id: "category", label: "Category", sort: "category", render: t => `<td>${esc(incoCat(t.category))}</td>` },
        { id: "tags", label: "Tags", sort: null, render: t => `<td>${(t.tags && t.tags.length) ? esc(t.tags.join(", ")) : "-"}</td>` },
        { id: "added_time", label: "Added", sort: "added_time", render: t => `<td>${formatDate(t.added_time)}</td>` },
        { id: "completed_time", label: "Completed", sort: "completed_time", render: t => `<td>${formatDate(t.completed_time)}</td>` },
        { id: "agent", label: "Agent", sort: "agent", render: t => `<td>${esc(t.agent || "local")}</td>` },
    ],
    "race-table": [
        { id: "name", label: "Name", sort: "name", render: t => `<td title="${t.info_hash}">${esc(incoName(t))}${t.tracker_error ? ' <span class="tracker-warn" title="Tracker error">!</span>' : ''}${t.injected_peers ? ` <span class="uploader-badge ${t.injection_hit ? 'injection-hit' : ''}" title="Uploader: ${t.uploader} - ${t.injected_peers} peers injected${t.injection_hit ? ' HIT' : ''}">${t.injection_hit ? '&#9889;&#10003;' : '&#9889;'}${t.injected_peers}</span>` : ''}</td>` },
        { id: "total_size", label: "Size", sort: "total_size", render: t => `<td>${t.total_size ? formatBytes(t.total_size) : "-"}</td>` },
        { id: "progress", label: "Progress", sort: "progress", render: t => { const pct = (t.progress * 100).toFixed(1); return `<td><div class="progress-bar"><div class="progress-fill" style="width:${pct}%"></div></div><div class="progress-text">${pct}%</div></td>`; } },
        { id: "swarm_seeds", label: "Seeds", sort: "swarm_seeds", render: t => `<td>${t.swarm_seeds ?? "-"}</td>` },
        { id: "swarm_leechers", label: "Leechers", sort: "swarm_leechers", render: t => `<td>${t.swarm_leechers ?? "-"}</td>` },
        { id: "download_rate", label: "Down", sort: "download_rate", render: t => `<td>${formatSpeed(t.download_rate)}</td>` },
        { id: "upload_rate", label: "Up", sort: "upload_rate", render: t => `<td>${formatSpeed(t.upload_rate)}</td>` },
        { id: "ratio", label: "Ratio", sort: "ratio", render: t => `<td>${displayRatio(t).toFixed(2)}</td>` },
        { id: "tracker_host", label: "Tracker", sort: "tracker_host", render: t => `<td>${esc(t.tracker_host || "-")}</td>` },
        { id: "added_time", label: "Added", sort: "added_time", render: t => `<td>${formatDate(t.added_time)}</td>` },
        { id: "completed_time", label: "Completed", sort: "completed_time", render: t => `<td>${formatDate(t.completed_time)}</td>` },
        { id: "agent", label: "Agent", sort: "agent", render: t => `<td>${esc(t.agent || "local")}</td>` },
    ],
};
const _COL_SORTFN = { "hoard-table": "sortHoard", "race-table": "sortRace" };

function _colCfg(tableId) {
    const ids = TABLE_COLS[tableId].map(c => c.id);
    let cfg = null;
    try { cfg = JSON.parse(localStorage.getItem("hydra_colcfg_" + tableId) || "null"); } catch (_) { }
    if (!cfg || !Array.isArray(cfg.order)) cfg = { order: ids.slice(), hidden: [] };
    const known = new Set(ids);
    cfg.order = cfg.order.filter(id => known.has(id));
    ids.forEach(id => { if (!cfg.order.includes(id)) cfg.order.push(id); });
    cfg.hidden = Array.isArray(cfg.hidden) ? cfg.hidden.filter(id => known.has(id)) : [];
    return cfg;
}
function _colSaveCfg(tableId, cfg) { localStorage.setItem("hydra_colcfg_" + tableId, JSON.stringify(cfg)); }
function _visibleCols(tableId) {
    const cfg = _colCfg(tableId);
    const hidden = new Set(cfg.hidden);
    const byId = {}; TABLE_COLS[tableId].forEach(c => byId[c.id] = c);
    return cfg.order.map(id => byId[id]).filter(c => c && !hidden.has(c.id));
}
// Row cells for a torrent in the current column order (used by the renderers).
function renderRowCells(tableId, t) {
    return _visibleCols(tableId).map(c => c.render(t)).join("");
}
function renderTableHeader(tableId, sortCol, sortAsc) {
    const thead = document.querySelector("#" + tableId + " thead");
    if (!thead) return;
    const fn = _COL_SORTFN[tableId];
    thead.innerHTML = "<tr>" + _visibleCols(tableId).map(c => {
        const sortable = c.sort ? ` data-col="${c.sort}" onclick="${fn}(this)"` : "";
        const cls = (c.sort && c.sort === sortCol) ? (sortAsc ? " sort-asc" : " sort-desc") : "";
        return `<th class="col-drag${cls}" draggable="true" data-colid="${c.id}"${sortable}>${esc(t(c.label))}</th>`;
    }).join("") + "</tr>";
    _wireHeaderDnD(tableId);
    // Re-attach the column-width resizers (the innerHTML rebuild dropped them).
    const _rt = document.getElementById(tableId);
    if (_rt) { _rt._colResizeInit = false; initResizableColumns(_rt, tableId === "hoard-table" ? "hydra_cols_hoard" : "hydra_cols_race"); }
}
let _colDragId = null;
function _wireHeaderDnD(tableId) {
    const thead = document.querySelector("#" + tableId + " thead");
    if (!thead) return;
    thead.querySelectorAll("th").forEach(th => {
        th.addEventListener("dragstart", e => { _colDragId = th.dataset.colid; e.dataTransfer.effectAllowed = "move"; });
        th.addEventListener("dragover", e => { e.preventDefault(); e.dataTransfer.dropEffect = "move"; th.classList.add("col-drop"); });
        th.addEventListener("dragleave", () => th.classList.remove("col-drop"));
        th.addEventListener("drop", e => {
            e.preventDefault(); th.classList.remove("col-drop");
            const target = th.dataset.colid;
            if (!_colDragId || _colDragId === target) return;
            const cfg = _colCfg(tableId);
            const from = cfg.order.indexOf(_colDragId), to = cfg.order.indexOf(target);
            if (from < 0 || to < 0) return;
            cfg.order.splice(to, 0, cfg.order.splice(from, 1)[0]);
            _colSaveCfg(tableId, cfg);
            _rerenderTable(tableId);
        });
    });
}
function _rerenderTable(tableId) {
    if (tableId === "hoard-table") { renderTableHeader("hoard-table", _hoardSortCol, _hoardSortAsc); renderHoardTable(); }
    else if (tableId === "race-table") { renderTableHeader("race-table", _raceSortCol, _raceSortAsc); updateRaceTorrents(); }
}
function showColumnMenu(ev, tableId) {
    ev.preventDefault();
    document.querySelectorAll(".col-menu").forEach(m => m.remove());
    const cfg = _colCfg(tableId);
    const hidden = new Set(cfg.hidden);
    const byId = {}; TABLE_COLS[tableId].forEach(c => byId[c.id] = c);
    const menu = document.createElement("div");
    menu.className = "col-menu ctx-menu";
    menu.style.display = "block"; menu.style.position = "fixed"; menu.style.zIndex = "9999";
    menu.innerHTML = `<div class="ctx-label">${t("Columns")}</div><div class="ctx-separator"></div><div class="ctx-scroll">` +
        cfg.order.map(id => {
            const col = byId[id]; if (!col) return "";
            const on = !hidden.has(id);
            return `<div class="ctx-item" onclick="toggleColumn('${tableId}','${id}',this)"><span class="col-chk">${on ? "✓" : ""}</span>${esc(t(col.label))}</div>`;
        }).join("") + `</div>`;
    document.body.appendChild(menu);
    menu.style.left = Math.min(ev.clientX, window.innerWidth - 240) + "px";
    menu.style.top = Math.min(ev.clientY, window.innerHeight - menu.offsetHeight - 8) + "px";
}
function toggleColumn(tableId, id, el) {
    const cfg = _colCfg(tableId);
    const set = new Set(cfg.hidden);
    if (set.has(id)) set.delete(id); else set.add(id);
    cfg.hidden = [...set];
    _colSaveCfg(tableId, cfg);
    _rerenderTable(tableId);
    if (el) { const chk = el.querySelector(".col-chk"); if (chk) chk.textContent = set.has(id) ? "" : "✓"; }
}
document.addEventListener("click", e => {
    if (!e.target.closest(".col-menu")) document.querySelectorAll(".col-menu").forEach(m => m.remove());
});
(function initTableColumns() {
    const wire = () => {
        renderTableHeader("hoard-table", _hoardSortCol, _hoardSortAsc);
        renderTableHeader("race-table", _raceSortCol, _raceSortAsc);
        ["hoard-table", "race-table"].forEach(tid => {
            const thead = document.querySelector("#" + tid + " thead");
            if (thead && !thead.dataset.colMenuWired) {
                thead.dataset.colMenuWired = "1";
                thead.addEventListener("contextmenu", e => showColumnMenu(e, tid));
            }
        });
    };
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", wire);
    else wire();
})();

// ─── Changelog (renders the repo CHANGELOG.md, single source) ───────────────
function _renderMarkdown(md) {
    const esc = t => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    const inline = t => esc(t)
        .replace(/`([^`]+)`/g, "<code>$1</code>")
        .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
        .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>')
        .replace(/\\([\\`*_[\]()#+\-.!])/g, "$1");

    // Fold soft-wrapped lines into their block BEFORE formatting anything. A
    // paragraph or a list item written across several lines is one block, and
    // rendering line by line broke both halves of that: the continuation lines
    // escaped their <li> and became loose paragraphs, and emphasis opened at the
    // end of one line never found its closing ** on the next, so the markers
    // showed up as literal text. Any changelog wrapped at 80 columns hit this.
    const blocks = [];
    for (const raw of md.split("\n")) {
        const line = raw.replace(/\s+$/, "").trim();
        if (line === "") { blocks.push({ type: "blank" }); continue; }
        if (/^#{1,6} /.test(line)) { blocks.push({ type: "head", text: line }); continue; }
        if (/^---+$/.test(line)) { blocks.push({ type: "hr" }); continue; }
        if (/^[-*] /.test(line)) { blocks.push({ type: "li", text: line.slice(2) }); continue; }
        const prev = blocks[blocks.length - 1];
        if (prev && (prev.type === "li" || prev.type === "p")) { prev.text += " " + line; continue; }
        blocks.push({ type: "p", text: line });
    }

    const out = [];
    let inList = false;
    const closeList = () => { if (inList) { out.push("</ul>"); inList = false; } };
    for (const b of blocks) {
        if (b.type === "li") {
            if (!inList) { out.push("<ul>"); inList = true; }
            out.push("<li>" + inline(b.text) + "</li>");
            continue;
        }
        // A blank line between two bullets is a loose list, not two lists —
        // only a heading, rule or paragraph actually ends one.
        if (b.type === "blank") { continue; }
        closeList();
        if (b.type === "head") {
            const lvl = b.text.match(/^#+/)[0].length;
            out.push("<h" + lvl + ">" + inline(b.text.slice(lvl + 1)) + "</h" + lvl + ">");
        } else if (b.type === "hr") { out.push("<hr>"); }
        else if (b.type === "p") { out.push("<p>" + inline(b.text) + "</p>"); }
    }
    closeList();
    return out.join("\n");
}
async function loadChangelog() {
    const el = document.getElementById("changelog-body");
    if (!el || el.dataset.loaded) return;
    try {
        const res = await fetch("/changelog.md", { cache: "no-cache" });
        if (!res.ok) throw new Error("HTTP " + res.status);
        el.innerHTML = _renderMarkdown(await res.text());
        el.dataset.loaded = "1";
    } catch (e) { el.textContent = t("Failed to load changelog: {msg}", { msg: e.message }); }
}

let _lastUpdateCheck = 0;
async function checkForUpdate() {
    const el = document.getElementById("update-badge");
    if (!el) return;
    // Server caches the GitHub lookup 6h, so polling it every health tick is
    // pointless, throttle the client to once per 30 min.
    if (_lastUpdateCheck && Date.now() - _lastUpdateCheck < 1800000) return;
    _lastUpdateCheck = Date.now();
    try {
        const d = await api("/api/update-check");
        if (d && d.enabled && d.update_available) {
            el.innerHTML = ` <a href="${d.url || '#'}" target="_blank" rel="noopener" class="update-badge" title="${t("A newer version ({v}) is available", { v: esc(d.latest) })}">${t("update {v}", { v: esc(d.latest) })}</a>`;
        } else {
            el.innerHTML = "";
        }
    } catch (_) {}
}


// ─── Logs tab (in-process hub: filter, live-tail SSE, copy/export/issue) ────
let logsEntries = [];
let logsTailSource = null;
let _logsInit = false;

function logsFilters() {
    return {
        source: document.getElementById("logs-source").value,
        level: document.getElementById("logs-level").value,
        since: document.getElementById("logs-since").value,
        q: document.getElementById("logs-q").value.trim(),
    };
}
function fmtLogLine(e) {
    const ts = String(e.ts || "").replace("T", " ").replace("Z", "").slice(0, 23);
    return `${ts}  ${String(e.level || "").padEnd(5)} ${String(e.source || "").padEnd(13)} ${e.msg || ""}`;
}
function renderLogs() {
    const el = document.getElementById("logs-body");
    if (!el) return;
    const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 20;
    el.textContent = logsEntries.length ? logsEntries.map(fmtLogLine).join("\n") : t("(no matching entries)");
    if (atBottom) el.scrollTop = el.scrollHeight;
    updateIssueLink();
}
function logsQuery() {
    const f = logsFilters();
    const qs = new URLSearchParams();
    if (f.source) qs.set("source", f.source);
    if (f.level) qs.set("level", f.level);
    if (f.since) qs.set("since", f.since);
    if (f.q) qs.set("q", f.q);
    return qs;
}
async function loadLogs() {
    if (!_logsInit) {
        _logsInit = true;
        ["logs-source", "logs-level", "logs-since"].forEach(id =>
            document.getElementById(id).addEventListener("change", onLogsFilterChange));
        document.getElementById("logs-q").addEventListener("input", onLogsFilterChange);
        document.getElementById("logs-tail").addEventListener("change", (ev) => {
            if (ev.target.checked) startLogsTail(); else stopLogsTail();
        });
    }
    const qs = logsQuery();
    qs.set("limit", "3000");
    try {
        const d = await api("/api/logs?" + qs.toString());
        logsEntries = d.entries || [];
        renderLogs();
    } catch (e) {
        const el = document.getElementById("logs-body");
        if (el) el.textContent = t("Failed to load logs: {msg}", { msg: e.message });
    }
}
function onLogsFilterChange() {
    loadLogs().then(() => { if (document.getElementById("logs-tail").checked) startLogsTail(); });
}
function startLogsTail() {
    stopLogsTail();
    const qs = logsQuery();
    qs.set("apikey", API_KEY);
    logsTailSource = new EventSource("/api/logs/stream?" + qs.toString());
    logsTailSource.onmessage = (ev) => {
        try {
            logsEntries.push(JSON.parse(ev.data));
            if (logsEntries.length > 5000) logsEntries.splice(0, logsEntries.length - 5000);
            renderLogs();
        } catch {}
    };
}
function stopLogsTail() {
    if (logsTailSource) { logsTailSource.close(); logsTailSource = null; }
}
function logsText() { return logsEntries.map(fmtLogLine).join("\n"); }
function copyLogs() {
    navigator.clipboard.writeText(logsText());
    const el = document.getElementById("logs-body");
    if (el) { const p = el.style.borderColor; el.style.borderColor = "#3fb950"; setTimeout(() => el.style.borderColor = p, 400); }
}
function exportLogs() {
    const blob = new Blob([logsText()], { type: "text/plain" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = "hydra-logs.txt";
    a.click();
    URL.revokeObjectURL(a.href);
}
function updateIssueLink() {
    const a = document.getElementById("logs-issue");
    if (!a) return;
    const body = "**Describe the issue**\n\n\n**Version:** (see the startup banner)\n\n**Logs** (paste from the Logs tab, check for your public IP before posting):\n```\n\n```\n";
    a.href = "https://github.com/Kheopsian/Hydra/issues/new?title=" + encodeURIComponent("[bug] ") + "&body=" + encodeURIComponent(body);
}

// ─── Startup pause ──────────────────────────────────────
//
// While `start_paused` holds an engine, Hydra sends no announces and dials no
// peers. That is indistinguishable from a broken daemon from the outside, so
// the banner is permanent and undismissable for as long as the hold lasts.
// The hold is process-level: releasing it does not touch any torrent's own
// paused state, so torrents the user paused by hand stay paused.

let _startupPauseTimer = null;

async function refreshStartupPause() {
    const el = document.getElementById("startup-pause-banner");
    if (!el) return;
    let st;
    try {
        st = await api("/api/startup-pause");
    } catch (_) {
        // Daemon unreachable: leave whatever is on screen rather than implying
        // the hold was lifted.
        return;
    }
    if (!st || !st.holding) {
        el.style.display = "none";
        if (_startupPauseTimer) { clearInterval(_startupPauseTimer); _startupPauseTimer = null; }
        return;
    }
    const engines = (st.held || []).join(", ");
    // textContent, so no esc(): escaping here would put the entities on screen.
    document.getElementById("startup-pause-title").textContent =
        window.t("Startup pause: {engines} not announcing", { engines });
    document.getElementById("startup-pause-text").textContent =
        window.t("No announces or peer connections are leaving Hydra. Adjust your rate limits now if you need to, then start. Torrents you paused yourself stay paused.");
    el.style.display = "block";
    // Another browser tab, or the API, can release it: keep checking so this
    // banner cannot outlive the hold it describes.
    if (!_startupPauseTimer) _startupPauseTimer = setInterval(refreshStartupPause, 10000);
}

async function releaseStartupPause() {
    const btn = document.getElementById("startup-pause-release");
    if (btn) { btn.disabled = true; btn.textContent = window.t("Starting…"); }
    try {
        await api("/api/startup-pause/release", { method: "POST" });
    } catch (e) {
        if (btn) { btn.disabled = false; btn.textContent = window.t("Start now"); }
        hydraNotify(window.t("Error: {msg}", { msg: e.message }));
        return;
    }
    await refreshStartupPause();
    if (btn) { btn.disabled = false; btn.textContent = window.t("Start now"); }
}

window.addEventListener("DOMContentLoaded", refreshStartupPause);

// ---- Network mode tab ------------------------------------------------------
// The flat settings list cannot say the one thing that matters about these
// keys: the setups are mutually exclusive. Nobody runs a VPN and a SOCKS5
// relay at once, but the config file holds the keys of both, so an abandoned
// attempt looks exactly like a deliberate setup. Here you pick one, you see
// only its fields, and saving clears the others.
const NET_MODES = [
    { id: "direct", label: "Direct",
      blurb: "No proxy. Each engine leaves by the interface you give it, or by the host's default route when you give it none. This is where a bare-metal WireGuard tunnel goes, one per engine if you want." },
    { id: "gluetun", label: "Gluetun",
      blurb: "A gluetun container holds the tunnel and hands out a forwarded port. Hydra reads that port and follows it." },
    { id: "socks5", label: "SOCKS5 proxy",
      blurb: "Outgoing connections go through a SOCKS5 proxy: peer dials and tracker announces both. Nobody can connect to you from outside." },
    { id: "proxy_v2", label: "SOCKS5 + PROXY-v2 relay",
      blurb: "As above, plus a relay that forwards incoming peers with their real address in a PROXY-v2 header." },
];

let _netState = null;

function _netField(id, label, type, value, hint, extra) {
    const attrs = extra || "";
    const v = value === undefined || value === null ? "" : String(value);
    return `<div class="settings-row">
        <div class="sr-label"><span class="sr-key">${esc(t(label))}</span>${hint ? `<span class="sr-desc">${esc(t(hint))}</span>` : ""}</div>
        <div class="sr-field"><input type="${type}" class="sr-input" id="${id}" value="${esc(v)}" ${attrs}></div>
    </div>`;
}

function _netSelect(id, label, value, options, hint) {
    // Keep a configured value that no longer matches any interface: dropping it
    // would silently rewrite the config the moment the page is saved.
    const opts = options.slice();
    if (value && !opts.includes(value)) opts.push(value);
    let html = `<option value=""${value ? "" : " selected"}>${esc(t("Select an interface"))}</option>`;
    for (const o of opts) html += `<option value="${esc(o)}"${o === value ? " selected" : ""}>${esc(o)}</option>`;
    return `<div class="settings-row">
        <div class="sr-label"><span class="sr-key">${esc(t(label))}</span>${hint ? `<span class="sr-desc">${esc(t(hint))}</span>` : ""}</div>
        <div class="sr-field"><select class="sr-input" id="${id}">${html}</select></div>
    </div>`;
}

function _netEngineSelect(id, label, value, hint) {
    // Two fixed choices, so no empty option: there is no such thing as
    // "no engine" once the forwarded port is being followed.
    const cur = value === "race" ? "race" : "hoard";
    let html = "";
    for (const o of [["hoard", "Hoard"], ["race", "Race"]]) {
        html += `<option value="${o[0]}"${o[0] === cur ? " selected" : ""}>${esc(t(o[1]))}</option>`;
    }
    return `<div class="settings-row">
        <div class="sr-label"><span class="sr-key">${esc(t(label))}</span>${hint ? `<span class="sr-desc">${esc(t(hint))}</span>` : ""}</div>
        <div class="sr-field"><select class="sr-input" id="${id}">${html}</select></div>
    </div>`;
}

function _netCheckbox(id, label, checked, hint) {
    return `<div class="settings-row">
        <div class="sr-label"><span class="sr-key">${esc(t(label))}</span>${hint ? `<span class="sr-desc">${esc(t(hint))}</span>` : ""}</div>
        <div class="sr-field"><label class="toggle"><input type="checkbox" id="${id}" ${checked ? "checked" : ""}><span class="toggle-track"></span></label></div>
    </div>`;
}

function _netPortsHTML(f) {
    let extras = "";
    for (const e of _netExtras()) {
        extras += _netField("net-extra-port-" + e.id, t("{id} listen port", { id: e.id }), "number", e.listen_port,
            t("Port this engine accepts peers on. It has to differ from every other engine's."));
    }
    return _netField("net-race-port", "Race listen port", "number", f.race_listen_port, "Port the race engine accepts peers on.")
         + _netField("net-hoard-port", "Hoard listen port", "number", f.hoard_listen_port, "Port the hoard engine accepts peers on. It has to differ from the race one.")
         + extras
         + _netCheckbox("net-ipv6", "Listen over IPv6 too", f.enable_ipv6, "Only if this host really has working IPv6. Announcing an address nobody can reach costs you peers.");
}

// _netExtras are this node's engines beyond the two primaries. The page showed
// exactly two rows, so an engine added from the Agents menu had nowhere to be
// configured: it ran on a copy of its role's primary and could never be given a
// tunnel of its own, which was the whole point of a per-engine interface.
function _netExtras() {
    return (_netState && _netState.extra_engines) || [];
}

function _netSocksHTML(f) {
    return `<div class="settings-section"><div class="settings-section-title">${t("SOCKS5 proxy")}</div>
        <p class="sr-desc" style="margin:.2em 0 .8em">${t("This proxy carries everything that leaves Hydra: the connections to peers and the announces to trackers. Both go through it, so neither can reveal your real address.")}</p>
        ${_netField("net-socks-host", "Proxy host", "text", f.socks5_host, "IP or hostname of the SOCKS5 server.")}
        ${_netField("net-socks-port", "Proxy port", "number", f.socks5_port || "", "")}
        ${_netField("net-socks-user", "Username", "text", f.socks5_user, "Leave both credentials empty for an open proxy.")}
        ${_netField("net-socks-pass", "Password", "password", f.socks5_pass, "")}
    </div>`;
}

function netModePanelHTML() {
    return `<div class="settings-panel" data-domain="__network">
        <div id="net-mode-body"><p class="sr-desc">${t("Loading…")}</p></div>
    </div>`;
}

async function netModeInit() {
    const body = document.getElementById("net-mode-body");
    if (!body) return;
    try {
        _netState = await api("/api/network/mode");
    } catch (e) {
        body.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
        return;
    }
    _netOrig = JSON.stringify({ mode: _netState.mode, fields: _netState.fields });
    netModeRender();
}

function netModeSelect(mode) {
    if (!_netState) return;
    _netState.fields = netModeCollect() || _netState.fields;
    _netState.mode = mode;
    netModeRender();
}

function netModeRender() {
    const body = document.getElementById("net-mode-body");
    if (!body || !_netState) return;
    const f = _netState.fields || {};
    const mode = _netState.mode;

    let cards = "";
    for (const m of NET_MODES) {
        cards += `<button type="button" class="net-mode-card${m.id === mode ? " active" : ""}" onclick="netModeSelect('${m.id}')">
            <span class="nm-title">${esc(t(m.label))}</span>
            <span class="nm-blurb">${esc(t(m.blurb))}</span>
        </button>`;
    }

    let fields = "";
    if (mode === "direct" || mode === "gluetun") {
        // One interface per engine. The two engines are independent network
        // identities, and a single shared field could not say so: it put both
        // on one tunnel while the page implied otherwise.
        const list = (_detectedIfaces || []).map(i => (i.name || i));
        const hint = "The interface this engine leaves by. Peer connections AND tracker announces are both bound to it, so neither can travel outside it. Empty means the host's default route.";
        fields += `<div class="settings-section"><div class="settings-section-title">${t("Interface per engine")}</div>
            ${_netSelect("net-race-iface", "Race engine interface", f.race_bind_interface, list, hint)}
            ${_netSelect("net-hoard-iface", "Hoard engine interface", f.hoard_bind_interface, list, hint)}
            ${_netExtras().map(e => _netSelect("net-extra-iface-" + e.id,
                t("{id} engine interface ({role})", { id: e.id, role: e.role }), e.bind_interface, list, hint)).join("")}
            <p class="sr-desc">${t("Give the engines different tunnels to spread them across several exit addresses, or the same one to keep them together. Leave them empty on a host with no VPN.")}</p>
        </div>`;
    }
    if (mode === "gluetun") {
        fields += `<div class="settings-section"><div class="settings-section-title">${t("Gluetun")}</div>
            ${_netCheckbox("net-gluetun", "Take the listen port from gluetun", f.gluetun_port_forward, "Gluetun asks your provider for a forwarded port and that port changes with the lease. Hydra reads it, listens there, and follows it. Announces are held at startup until it knows the port, so no tracker is ever handed the wrong one.")}
            ${_netField("net-gluetun-url", "Gluetun control server", "text", f.gluetun_url, "Empty means http://127.0.0.1:8000, which is right when Hydra shares gluetun's network.")}
            ${_netField("net-gluetun-key", "Gluetun API key", "password", f.gluetun_api_key, "Recent gluetun versions refuse every request without one. The role needs the route GET /v1/portforward.")}
            ${_netEngineSelect("net-gluetun-engine", "Engine that takes the port", f.gluetun_port_engine, "A provider forwards a single port, so one engine gets it. Hoard seeds around the clock, so being reachable pays off continuously; race needs peers fast on a fresh torrent.")}
            <p class="sr-desc">${t("The other engine keeps its own listen port and stays unreachable.")}</p>
        </div>`;
    }
    if (mode === "socks5" || mode === "proxy_v2") fields += _netSocksHTML(f);
    if (mode === "proxy_v2") {
        fields += `<div class="settings-section"><div class="settings-section-title">${t("Incoming relay")}</div>
            <p class="sr-desc" style="margin:.2em 0 .8em">${t("Your relay forwards peers to these ports with a PROXY-v2 header, so the real peer address survives the hop.")}</p>
            ${_netField("net-race-pv2", "Race PROXY-v2 port", "number", f.race_proxy_v2_port || "", "0 to leave this engine without a PROXY-v2 listener.")}
            ${_netField("net-hoard-pv2", "Hoard PROXY-v2 port", "number", f.hoard_proxy_v2_port || "", "")}
            ${_netField("net-pv2-addr", "Bind address", "text", f.proxy_v2_listen_addr, "Empty means every address.")}
            ${_netField("net-pv2-trusted", "Trusted sources", "text", (f.proxy_v2_trusted_sources || []).join(", "), "Addresses allowed to send PROXY-v2 headers, comma separated. Required: with none, whoever reaches that port can claim to be any peer.")}
        </div>`;
    }
    fields += `<div class="settings-section"><div class="settings-section-title">${t("Ports")}</div>${_netPortsHTML(f)}</div>`;

    let warn = "";
    for (const w of (_netState.warnings || [])) warn += `<div class="result-msg info" style="margin:.3em 0">${esc(t(w))}</div>`;

    let env = "";
    if ((_netState.env_overrides || []).length) {
        let rows = "";
        for (const e of _netState.env_overrides) {
            rows += `<div class="settings-row"><div class="sr-label"><span class="sr-key">${esc(e.name)}</span><span class="sr-desc">${esc(t(e.effect))}</span></div><div class="sr-field"><code>${esc(e.value)}</code></div></div>`;
        }
        env = `<div class="settings-section"><div class="settings-section-title">${t("Set in the environment")}</div>
            <p class="sr-desc" style="margin:.2em 0 .8em">${t("These come from the container or service environment, not from this page, and they take precedence over it.")}</p>${rows}</div>`;
    }

    body.innerHTML = `<div class="net-mode-cards">${cards}</div>
        ${warn}
        ${fields}
        ${env}
        <div style="margin:1em 0;display:flex;gap:8px;flex-wrap:wrap">
            <button class="btn-primary" onclick="netModeSave()">${t("Save this mode")}</button>
            <button class="btn-small" id="net-check-btn" onclick="netModeCheck()">${t("Check what actually happens")}</button>
        </div>
        <div id="net-mode-result"></div>`;
    // The WireGuard half loads on its own: it asks the daemon what the tunnels
    // are doing right now, which is a different question from what the config
    // file says, and the two are worth seeing side by side.
    if (mode === "direct") {
        const host = document.createElement("div");
        host.id = "net-wg-body";
        body.appendChild(host);
        netWgLoad();
    }
}

function netModeCollect() {
    const num = id => { const el = document.getElementById(id); return el ? Number(el.value || 0) : 0; };
    const str = id => { const el = document.getElementById(id); return el ? el.value.trim() : ""; };
    const bool = id => { const el = document.getElementById(id); return el ? !!el.checked : false; };
    if (!document.getElementById("net-race-port")) return null;
    const prev = (_netState && _netState.fields) || {};
    return {
        race_listen_port: num("net-race-port"),
        hoard_listen_port: num("net-hoard-port"),
        enable_ipv6: bool("net-ipv6"),
        race_bind_interface: document.getElementById("net-race-iface") ? str("net-race-iface") : (prev.race_bind_interface || ""),
        hoard_bind_interface: document.getElementById("net-hoard-iface") ? str("net-hoard-iface") : (prev.hoard_bind_interface || ""),
        socks5_host: document.getElementById("net-socks-host") ? str("net-socks-host") : (prev.socks5_host || ""),
        socks5_port: document.getElementById("net-socks-port") ? num("net-socks-port") : (prev.socks5_port || 0),
        socks5_user: document.getElementById("net-socks-user") ? str("net-socks-user") : (prev.socks5_user || ""),
        socks5_pass: document.getElementById("net-socks-pass") ? str("net-socks-pass") : (prev.socks5_pass || ""),
        race_proxy_v2_port: document.getElementById("net-race-pv2") ? num("net-race-pv2") : (prev.race_proxy_v2_port || 0),
        hoard_proxy_v2_port: document.getElementById("net-hoard-pv2") ? num("net-hoard-pv2") : (prev.hoard_proxy_v2_port || 0),
        proxy_v2_listen_addr: document.getElementById("net-pv2-addr") ? str("net-pv2-addr") : (prev.proxy_v2_listen_addr || ""),
        gluetun_port_forward: document.getElementById("net-gluetun") ? bool("net-gluetun") : !!prev.gluetun_port_forward,
        gluetun_url: document.getElementById("net-gluetun-url") ? str("net-gluetun-url") : (prev.gluetun_url || ""),
        gluetun_api_key: document.getElementById("net-gluetun-key") ? str("net-gluetun-key") : (prev.gluetun_api_key || ""),
        gluetun_port_engine: document.getElementById("net-gluetun-engine") ? str("net-gluetun-engine") : (prev.gluetun_port_engine || "hoard"),
        proxy_v2_trusted_sources: document.getElementById("net-pv2-trusted")
            ? str("net-pv2-trusted").split(",").map(s => s.trim()).filter(Boolean)
            : (prev.proxy_v2_trusted_sources || []),
    };
}

// netModeCollectExtras reads the per-engine rows back. Absent inputs mean the
// mode does not show them, so the engine keeps what it has rather than being
// silently blanked by a save made from another mode.
function netModeCollectExtras() {
    return _netExtras().map(function (e) {
        const iface = document.getElementById("net-extra-iface-" + e.id);
        const port = document.getElementById("net-extra-port-" + e.id);
        return {
            id: e.id,
            role: e.role,
            bind_interface: iface ? iface.value.trim() : (e.bind_interface || ""),
            listen_port: port ? Number(port.value || 0) : (e.listen_port || 0),
        };
    });
}

async function netModeSave() {
    const out = document.getElementById("net-mode-result");
    const fields = netModeCollect();
    if (!fields) return;
    _netState.fields = fields;
    const extra_engines = netModeCollectExtras();
    _netState.extra_engines = extra_engines;
    try {
        const r = await api("/api/network/mode", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ mode: _netState.mode, fields, extra_engines }),
        });
        let extra = "";
        for (const w of (r.warnings || [])) extra += `<div class="result-msg info" style="margin:.3em 0">${esc(t(w))}</div>`;
        _netOrig = JSON.stringify({ mode: _netState.mode, fields: fields });
        // Only a listen port still needs a restart -- it is the one setting a
        // running engine keeps across a config apply. Everything else on this
        // page reaches the engines in seconds, and a banner shown anyway taught
        // people to restart for changes that were already live.
        if (r.restart_required) {
            _setRestartBanner(t("Saved. The engines need a restart to pick up the new listen port.") +
                ` <button class="btn-small btn-danger" onclick="restartDaemon()" style="margin-left:8px">${t("Apply &amp; restart")}</button>`);
        } else {
            extra = `<div class="result-msg success" style="margin:.3em 0">${esc(t("Saved and applied, no restart needed."))}</div>` + extra;
        }
        out.innerHTML = extra;
    } catch (e) {
        out.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
    }
}

async function netModeCheck() {
    const out = document.getElementById("net-mode-result");
    const btn = document.getElementById("net-check-btn");
    // The probes take tens of seconds. Without the button changing, the click
    // looks like it did nothing at all, so freeze it for the whole run.
    const label = btn ? btn.textContent : "";
    if (btn) { btn.disabled = true; btn.textContent = t("Checking…"); }
    out.innerHTML = `<div class="result-msg info">${t("Measuring… this takes up to a minute: it asks an outside service where our traffic comes from, then knocks on the port a tracker publishes for us.")}</div>`;
    out.scrollIntoView({ block: "nearest" });
    try {
        const r = await api("/api/network/check", {
            method: "POST", headers: { "Content-Type": "application/json" }, body: "{}",
        });
        let rows = "";
        for (const res of (r.results || [])) {
            const cls = res.status === "ok" ? "net-ok" : (res.status === "fail" ? "net-fail" : "net-warn");
            rows += `<div class="settings-row"><div class="sr-label"><span class="sr-key ${cls}">${esc(t(res.label))}</span></div>
                <div class="sr-field"><span class="${cls}">${esc(t(res.detail))}</span></div></div>`;
        }
        out.innerHTML = `<div class="settings-section"><div class="settings-section-title">${t("What actually happens")}</div>${rows}</div>`;
    } catch (e) {
        out.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
    } finally {
        if (btn) { btn.disabled = false; btn.textContent = label; }
    }
}


// --- Jobs tab -------------------------------------------------------------
//
// A move can run for hours, so the one thing this view must get right is
// telling a job that is progressing apart from one that is stuck. It polls
// while it is on screen and stops when it is not: there is no point refreshing
// a table nobody is looking at, and a finished list does not change.

let _jobsTimer = null;

function startJobsPolling() {
    loadJobs();
    if (_jobsTimer === null) _jobsTimer = setInterval(loadJobs, 2000);
}

function stopJobsPolling() {
    if (_jobsTimer !== null) {
        clearInterval(_jobsTimer);
        _jobsTimer = null;
    }
}

// The torrent's name as the job recorded it. Jobs created before the name was
// stored fall back to the last path segment of the destination, which for a
// move is the release folder and so usually reads the same.
function jobName(j) {
    const p = j.params || {};
    if (p.name) return p.name;
    const target = p.target || "";
    const seg = target.split("/").filter(Boolean).pop();
    return seg || "-";
}

function _jobStateLabel(state) {
    switch (state) {
        case "pending": return t("Queued");
        case "running": return t("Running");
        case "verifying": return t("Verifying");
        case "done": return t("Done");
        case "failed": return t("Failed");
        case "cancelled": return t("Cancelled");
        default: return state;
    }
}

async function loadJobs() {
    const tbody = document.getElementById("jobs-tbody");
    if (!tbody) return;
    let jobs = [];
    try {
        jobs = await api("/api/jobs?limit=200");
    } catch (err) {
        tbody.innerHTML = `<tr><td colspan="7">${t("Could not load jobs")}: ${String(err.message)}</td></tr>`;
        return;
    }
    if (!Array.isArray(jobs)) jobs = [];
    const activeOnly = document.getElementById("jobs-active-only");
    if (activeOnly && activeOnly.checked) {
        jobs = jobs.filter(j => j.state === "pending" || j.state === "running" || j.state === "verifying");
    }
    if (jobs.length === 0) {
        tbody.innerHTML = `<tr><td colspan="7">${t("Nothing running.")}</td></tr>`;
        return;
    }
    const esc = s => String(s == null ? "" : s).replace(/[&<>"']/g, ch =>
        ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#39;"}[ch]));
    tbody.innerHTML = jobs.map(j => {
        const pct = Math.min(100, Math.max(0, j.percent || 0));
        const running = j.state === "pending" || j.state === "running" || j.state === "verifying";
        const target = (j.params && j.params.target) || "";
        // The error is the whole story on a failed job, so it replaces the
        // progress bar rather than hiding in a tooltip.
        const progressCell = j.state === "failed"
            ? `<td class="job-error">${esc(j.error)}</td>`
            : `<td><div class="progress-bar"><div class="progress-fill${running ? " downloading" : ""}" style="width:${pct.toFixed(1)}%"></div></div>` +
              `<div class="progress-text">${pct.toFixed(1)}% ${j.total_bytes ? "(" + formatBytes(j.progress_bytes) + " / " + formatBytes(j.total_bytes) + ")" : ""}</div></td>`;
        const action = running
            ? `<td><button class="btn btn-small" onclick="cancelJob('${esc(j.id)}')">${t("Cancel")}</button></td>`
            : "<td></td>";
        return `<tr>` +
            `<td>${esc(_jobStateLabel(j.state))}</td>` +
            `<td>${esc(j.type)}</td>` +
            `<td class="job-torrent"><div class="job-name">${esc(jobName(j))}</div>` +
            `<div class="job-hash">${esc(j.info_hash)}</div></td>` +
            progressCell +
            `<td title="${esc(target)}">${esc(target)}</td>` +
            `<td>${j.created_at ? new Date(j.created_at * 1000).toLocaleString() : ""}</td>` +
            action +
            `</tr>`;
    }).join("");
}

async function cancelJob(id) {
    if (!await hydraConfirm(t("Cancel this job? A move stops where it is; the data stays where the torrent is seeding from."))) return;
    try {
        const r = await fetch(`/api/jobs/${encodeURIComponent(id)}`, {
            method: "DELETE",
            headers: { "X-Api-Key": API_KEY },
        });
        if (!r.ok) {
            let msg = `HTTP ${r.status}`;
            try { const j = await r.json(); if (j && j.error) msg = j.error; } catch (_) { /* empty */ }
            hydraNotify(msg);
        }
    } catch (err) {
        hydraNotify(err.message);
    }
    loadJobs();
}

// ── Dialogs ─────────────────────────────────────────────────────────────────
//
// Hydra's own modal, never native hydraNotify()/await hydraConfirm(): those cannot be styled,
// they freeze the page, and an alert gives the reader no way out but OK, which
// is wrong for anything they might want to back out of.

let _hydraModalResolve = null;

function _hydraModalClose(value) {
    document.getElementById("hydra-modal").style.display = "none";
    const done = _hydraModalResolve;
    _hydraModalResolve = null;
    if (done) done(value);
}

// hydraDialog renders a title, a body and a set of buttons. Each button is
// {label, value, kind} where kind maps to the existing modal button styles.
// Returns the chosen value; Escape and a click on the backdrop resolve to null.
function hydraDialog(title, body, buttons) {
    return new Promise(resolve => {
        // A dialog opening over another must not strand the first one's caller.
        if (_hydraModalResolve) _hydraModalClose(null);
        _hydraModalResolve = resolve;

        document.getElementById("hydra-modal-title").textContent = title || "";
        const bodyEl = document.getElementById("hydra-modal-body");
        bodyEl.textContent = body || "";
        // Multi-line messages keep their line breaks rather than running on.
        bodyEl.style.whiteSpace = "pre-line";

        const actions = document.getElementById("hydra-modal-actions");
        actions.innerHTML = "";
        (buttons || [{ label: t("OK"), value: true, kind: "keep" }]).forEach(b => {
            const el = document.createElement("button");
            el.className = "btn-modal btn-" + (b.kind || "keep");
            el.textContent = b.label;
            el.onclick = () => _hydraModalClose(b.value);
            actions.appendChild(el);
        });

        const overlay = document.getElementById("hydra-modal");
        overlay.style.display = "";
        overlay.onclick = e => { if (e.target === overlay) _hydraModalClose(null); };
        document.addEventListener("keydown", function esc(e) {
            if (e.key === "Escape") {
                document.removeEventListener("keydown", esc);
                _hydraModalClose(null);
            }
        });
    });
}

// hydraNotify states something that has already happened: one way out.
// Called with a single argument it is a drop-in for hydraNotify().
function hydraNotify(title, body) {
    if (body === undefined) { body = title; title = t("Hydra"); }
    return hydraDialog(title, body, [{ label: t("OK"), value: true, kind: "keep" }]);
}

// hydraConfirm asks before acting, and can always be declined. Called with a
// single argument it is a drop-in for await hydraConfirm() -- except that it returns a
// promise, so every caller must await it.
function hydraConfirm(title, body, okLabel, danger) {
    if (body === undefined) { body = title; title = t("Confirm"); }
    return hydraDialog(title, body, [
        { label: okLabel || t("Confirm"), value: true, kind: danger ? "delete" : "keep" },
        { label: t("Cancel"), value: false, kind: "cancel" },
    ]).then(v => v === true);
}

// _agentAction runs one action on a torrent that lives on an agent. Local rows
// keep their own endpoints; only the remote ones need the node named.
function _agentAction(agent, engine, action, hash, extra) {
    return fetch(`/api/agents/${encodeURIComponent(agent)}/action`, {
        method: "POST",
        headers: { "X-Api-Key": API_KEY, "Content-Type": "application/json" },
        body: JSON.stringify(Object.assign({ engine: engine, action: action, info_hash: hash }, extra || {})),
    });
}

// ── Live refresh for agent rows ─────────────────────────────────────────────
//
// SSE carries this node's engines only, so rows on agents never move between
// hydrations. A short poll of the agent slice alone keeps them current without
// bringing back the full-list fetch that hydration replaced.
const AGENT_POLL_INTERVAL = 5000;
let _agentPollTimer = null;

async function _pollAgentRows() {
    if (!_ctxAgents.some(a => a.name && a.kind !== "local")) return;
    if (document.hidden) return;
    let rows;
    try {
        rows = await api("/api/agents/torrents");
    } catch (e) {
        return; // a failed poll is not evidence that anything changed
    }
    if (!Array.isArray(rows) || !_hoardAllTorrents) return;

    const byKey = new Map(_hoardAllTorrents.map(t => [_rowKey(t), t]));
    let changed = false;
    const seen = new Set();
    for (const r of rows) {
        if ((r.mode || "hoard") !== "hoard") continue;
        const k = _rowKey(r);
        seen.add(k);
        const cur = byKey.get(k);
        if (!cur) {
            _hoardAllTorrents.push(r);
            changed = true;
            continue;
        }
        // Upsert in place so sort order and selection survive the refresh.
        Object.assign(cur, r);
        changed = true;
    }
    // A torrent removed on an agent has to leave the list too.
    for (const [k, t] of byKey) {
        if (_isLocalAgent(t.agent)) continue;
        if (!seen.has(k)) {
            _hoardAllTorrents = _hoardAllTorrents.filter(x => _rowKey(x) !== k);
            changed = true;
        }
    }
    if (changed) {
        try { _renderHoardCounts(); } catch (_) {}
        _scheduleHoardRender();
    }
}

window.addEventListener("DOMContentLoaded", () => {
    if (_agentPollTimer) clearInterval(_agentPollTimer);
    _agentPollTimer = setInterval(_pollAgentRows, AGENT_POLL_INTERVAL);
});


// ── The engines' network: one polygon, one vertex per engine ────────────────
//
// Two labelled dots said "Race" and "Hoard" because a node was those two
// engines. It is now one agent per engine, each with its own interface and its
// own exit address, so the shape has to grow: N vertices on a circle, joined by
// a neutral ring. The ring is deliberately neutral -- an edge coloured like a
// link would suggest the engines talk to each other, and they do not.
const NET_STATE_COLOR = { ok: "#3ddc84", warn: "#f5a524", bad: "#f2555a", off: "#4a5568" };

function _netPolyMarkup(engines, size) {
    const n = engines.length;
    if (!n) return "";
    const r = size / 2 - 4, cx = size / 2, cy = size / 2;
    const pts = engines.map((e, i) => {
        // First vertex at the top, so the same fleet always draws the same way.
        const a = -Math.PI / 2 + i * 2 * Math.PI / n;
        return n === 1 ? [cx, cy] : [cx + r * Math.cos(a), cy + r * Math.sin(a)];
    });
    const d = pts.map((p, i) => (i ? "L" : "M") + p[0].toFixed(1) + "," + p[1].toFixed(1)).join(" ") + (n > 2 ? " Z" : "");
    const ring = n > 1 ? `<path class="np-edge" d="${d}"/>` : "";
    const rr = n <= 3 ? 4.6 : (n <= 6 ? 4 : 3.4);
    const dots = pts.map((p, i) => {
        const e = engines[i];
        const cls = e.state === "warn" ? "np-node np-warn" : "np-node";
        return `<g class="${cls}"><title>${esc(e.agent + " — " + _netStateWord(e.state))}</title>` +
            `<circle cx="${p[0].toFixed(1)}" cy="${p[1].toFixed(1)}" r="${rr}" fill="${NET_STATE_COLOR[e.state] || NET_STATE_COLOR.off}"/></g>`;
    }).join("");
    return `<svg width="${size}" height="${size}" viewBox="0 0 ${size} ${size}">${ring}${dots}</svg>`;
}

function _netStateWord(s) {
    return s === "ok" ? t("reachable")
        : s === "bad" ? t("unreachable")
        : s === "off" ? t("offline")
        : t("not established yet");
}

async function updateNetPoly() {
    const el = document.getElementById("net-poly");
    if (!el) return;
    let d;
    try { d = await api("/api/network/engines"); } catch (e) { return; }
    _netEngines = (d && d.engines) || [];
    if (!_netEngines.length) return;
    el.innerHTML = _netPolyMarkup(_netEngines, 30);
    const worst = _netEngines.some(e => e.state === "bad") ? "bad"
        : _netEngines.some(e => e.state === "off") ? "off"
        : _netEngines.some(e => e.state === "warn") ? "warn" : "ok";
    el.title = tp(_netEngines.length, "{n} engine", "{n} engines", { n: _netEngines.length })
        + " — " + _netStateWord(worst);

    // The address only when there is ONE. Several engines behind several
    // tunnels have several exits, and printing one of them would name an
    // address most of the traffic does not leave by.
    const exits = (d && d.exits) || [];
    if (exits.length === 1) {
        _renderHeaderIP(exits[0], d.exit_ip_v6 || "");
    } else if (exits.length > 1) {
        const hdr = document.getElementById("header-exit-ip");
        if (hdr) {
            hdr.textContent = tp(exits.length, "{n} exit", "{n} exits", { n: exits.length });
            hdr.title = exits.join("\n");
        }
    }
}

// The panel grows out of the polygon: same object, one opening it, the other
// being it. transform-origin is the polygon's centre expressed in the panel's
// own box, so the animation starts exactly where the click landed.
function openNetPanel(from) {
    const panel = document.getElementById("net-panel");
    const scrim = document.getElementById("net-panel-scrim");
    if (!panel || !from) return;
    const rows = document.getElementById("net-panel-rows");
    rows.innerHTML = _netEngines.map(e => {
        const where = e.bind_interface
            ? `<span class="nr-iface">${esc(e.bind_interface)}</span>`
            : t("default route");
        const port = e.listen_port ? " · " + t("port {port}", { port: e.listen_port }) : "";
        const ip = e.exit_ip ? esc(incoExitIP(e.exit_ip)) : "\u2014";
        const ip6 = e.exit_ip_v6 ? "<br>" + esc(incoExitIP(e.exit_ip_v6)) : "";
        return `<div class="net-row">
            <div class="nr-dot" style="background:${NET_STATE_COLOR[e.state] || NET_STATE_COLOR.off}"></div>
            <div><div class="nr-name">${esc(e.agent)}</div>
                 <div class="nr-sub">${esc(e.role)} · ${where}${port}${e.detail ? " · " + esc(e.detail) : ""}</div></div>
            <div class="nr-ip">${ip}${ip6}</div>
        </div>`;
    }).join("") || `<div class="net-row"><div></div><div class="nr-sub">${t("No engine measured yet")}</div><div></div></div>`;

    const r = from.getBoundingClientRect();
    panel.style.visibility = "hidden";
    panel.classList.add("open");
    const h = panel.offsetHeight, w = panel.offsetWidth;
    panel.classList.remove("open");
    panel.style.visibility = "";
    const top = Math.min(r.bottom + 10, window.innerHeight - h - 12);
    const left = Math.min(Math.max(12, r.right - w), window.innerWidth - w - 12);
    panel.style.top = top + "px";
    panel.style.left = left + "px";
    panel.style.transformOrigin = `${(r.left + r.width / 2 - left).toFixed(0)}px ${(r.top + r.height / 2 - top).toFixed(0)}px`;
    requestAnimationFrame(() => { panel.classList.add("open"); scrim.classList.add("open"); });
}

function closeNetPanel() {
    const panel = document.getElementById("net-panel");
    const scrim = document.getElementById("net-panel-scrim");
    if (panel) panel.classList.remove("open");
    if (scrim) scrim.classList.remove("open");
}
document.addEventListener("keydown", e => { if (e.key === "Escape") closeNetPanel(); });

// --- WireGuard, managed by Hydra ------------------------------------------
//
// One engine, one configuration file, one tunnel. The page is built around
// that: there is no global tunnel to turn on, only a row per engine, because
// two engines sharing a tunnel leave by the same address with the same
// forwarded port, which is the arrangement this feature exists to replace.
//
// The .conf files are uploaded, never displayed. They hold a private key, and
// nothing here asks the API for their contents.

let _wgState = null;

async function netWgLoad() {
    const body = document.getElementById("net-wg-body");
    if (!body) return;
    try {
        _wgState = await api("/api/network/wireguard");
    } catch (e) {
        body.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
        return;
    }
    netWgRender();
}

function _wgEngineRows() {
    const rows = [{ id: "race", role: "race" }, { id: "hoard", role: "hoard" }];
    for (const e of _netExtras()) rows.push({ id: e.id, role: e.role });
    return rows;
}

function _wgTunnelFor(engineId) {
    return ((_wgState && _wgState.tunnels) || []).find(x => x.engine === engineId) || null;
}

function _wgAge(seconds) {
    if (!seconds && seconds !== 0) return t("never");
    if (seconds < 90) return t("{n}s ago", { n: Math.round(seconds) });
    return t("{n} min ago", { n: Math.round(seconds / 60) });
}

function netWgRender() {
    const body = document.getElementById("net-wg-body");
    if (!body || !_wgState) return;
    if (!_wgState.supported) {
        body.innerHTML = `<p class="sr-desc">${esc(t(_wgState.unsupported_reason || ""))}</p>`;
        return;
    }
    const files = _wgState.configs || [];
    const providers = _wgState.providers || [];

    let status = "";
    if ((_wgState.tunnels || []).length) {
        let rows = "";
        for (const tn of _wgState.tunnels) {
            const cls = tn.up ? "net-ok" : "net-fail";
            const port = tn.forwarded_port ? t("port {p}", { p: tn.forwarded_port }) : t("no incoming port");
            rows += `<div class="settings-row">
                <div class="sr-label"><span class="sr-key ${cls}">${esc(tn.engine)}</span>
                <span class="sr-desc">${esc(tn.device)} via ${esc(tn.provider_label || tn.provider || "")}</span></div>
                <div class="sr-field"><span class="${cls}">${esc(tn.up ? t("carrying traffic") : t("no recent handshake"))}</span>
                <span class="sr-desc">${esc(t("last handshake {age}", { age: _wgAge(tn.handshake_age_seconds) }))}, ${esc(port)}${tn.endpoint ? ", " + esc(tn.endpoint) : ""}</span>
                ${tn.note ? `<span class="sr-desc">${esc(t(tn.note))}</span>` : ""}
                ${tn.last_error ? `<span class="sr-desc net-fail">${esc(tn.last_error)}</span>` : ""}</div>
            </div>`;
        }
        status = `<div class="settings-section"><div class="settings-section-title">${t("Live tunnels")}</div>${rows}</div>`;
    }

    let fileRows = "";
    for (const f of files) {
        fileRows += `<div class="settings-row">
            <div class="sr-label"><span class="sr-key">${esc(f.name)}</span>
            <span class="sr-desc">${f.error ? esc(f.error) : esc((f.address || "") + (f.endpoint ? " to " + f.endpoint : ""))}</span></div>
            <div class="sr-field"><button class="btn-small btn-danger" onclick="netWgDeleteConfig('${esc(f.name)}')">${t("Remove")}</button></div>
        </div>`;
    }
    if (!fileRows) fileRows = `<p class="sr-desc">${t("No configuration file yet. Download one from your provider and add it here.")}</p>`;

    let engineRows = "";
    for (const e of _wgEngineRows()) {
        const cur = _wgCurrentFor(e.id);
        const fileOpts = [`<option value="">${t("none")}</option>`].concat(
            files.map(f => `<option value="${esc(f.name)}"${cur.config_file === f.name ? " selected" : ""}>${esc(f.name)}</option>`)).join("");
        const provOpts = providers.map(p =>
            `<option value="${esc(p.ID || p.id)}"${cur.provider === (p.ID || p.id) ? " selected" : ""}>${esc(p.Label || p.label)}</option>`).join("");
        const note = (providers.find(p => (p.ID || p.id) === cur.provider) || {});
        engineRows += `<div class="settings-row">
            <div class="sr-label"><span class="sr-key">${esc(e.id)}</span><span class="sr-desc">${esc(e.role)}</span></div>
            <div class="sr-field" style="display:flex;gap:6px;flex-wrap:wrap;align-items:center">
                <label><input type="checkbox" id="wg-en-${esc(e.id)}"${cur.enabled ? " checked" : ""}> ${t("tunnel")}</label>
                <select id="wg-file-${esc(e.id)}">${fileOpts}</select>
                <select id="wg-prov-${esc(e.id)}" onchange="netWgRenderNote('${esc(e.id)}')">${provOpts}</select>
                <input type="number" id="wg-port-${esc(e.id)}" placeholder="${t("port by hand")}" value="${cur.manual_port || ""}" style="width:8em">
                <span class="sr-desc" id="wg-note-${esc(e.id)}">${esc(t(note.Note || note.note || ""))}</span>
            </div>
        </div>`;
    }

    body.innerHTML = `${status}
        <div class="settings-section"><div class="settings-section-title">${t("Configuration files")}</div>
            <p class="sr-desc" style="margin:.2em 0 .8em">${t("A file is stored on this node and never shown again: it carries the tunnel's private key. Hydra reads the address, the peer and the keys from it, and decides the routing itself, so your host keeps its own default route.")}</p>
            ${fileRows}
            <div class="settings-row"><div class="sr-label"><span class="sr-key">${t("Add a file")}</span></div>
            <div class="sr-field"><input type="file" id="wg-upload" accept=".conf"> <button class="btn-small" onclick="netWgUpload()">${t("Upload")}</button></div></div>
        </div>
        <div class="settings-section"><div class="settings-section-title">${t("Tunnel per engine")}</div>
            <p class="sr-desc" style="margin:.2em 0 .8em">${t("Each engine gets its own file, so each leaves by its own address with its own forwarded port. Two engines cannot share one file.")}</p>
            ${engineRows}
            <div style="margin:.6em 0"><button class="btn-primary" onclick="netWgSave()">${t("Save the tunnels")}</button></div>
        </div>
        <div id="net-wg-result"></div>`;
}

// _wgCurrentFor reads what an engine runs today. The live tunnel is the truth
// when there is one: the config file may have been edited since.
function _wgCurrentFor(engineId) {
    const tn = _wgTunnelFor(engineId);
    const cfg = ((_wgState && _wgState.engines) || {})[engineId] || {};
    return {
        enabled: !!tn || !!cfg.enabled,
        provider: (tn && tn.provider) || cfg.provider || "proton",
        config_file: cfg.config_file || "",
        manual_port: cfg.manual_port || 0,
    };
}

function netWgRenderNote(engineId) {
    const sel = document.getElementById("wg-prov-" + engineId);
    const el = document.getElementById("wg-note-" + engineId);
    if (!sel || !el) return;
    const p = ((_wgState && _wgState.providers) || []).find(x => (x.ID || x.id) === sel.value) || {};
    el.textContent = t(p.Note || p.note || "");
}

async function netWgUpload() {
    const out = document.getElementById("net-wg-result");
    const input = document.getElementById("wg-upload");
    if (!input || !input.files || !input.files[0]) {
        out.innerHTML = `<div class="result-msg error">${t("Pick a .conf file first.")}</div>`;
        return;
    }
    const fd = new FormData();
    fd.append("file", input.files[0]);
    try {
        const r = await api("/api/network/wireguard/configs", { method: "POST", body: fd });
        out.innerHTML = `<div class="result-msg success">${esc(t(r.note || "Stored."))}</div>`;
        await netWgLoad();
    } catch (e) {
        out.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
    }
}

async function netWgDeleteConfig(name) {
    const out = document.getElementById("net-wg-result");
    if (!confirm(t("Remove {name}? The engine using it will not come up until another file is chosen.", { name }))) return;
    try {
        await api("/api/network/wireguard/configs/" + encodeURIComponent(name), { method: "DELETE" });
        await netWgLoad();
    } catch (e) {
        out.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
    }
}

async function netWgSave() {
    const out = document.getElementById("net-wg-result");
    const engines = _wgEngineRows().map(function (e) {
        const en = document.getElementById("wg-en-" + e.id);
        const file = document.getElementById("wg-file-" + e.id);
        const prov = document.getElementById("wg-prov-" + e.id);
        const port = document.getElementById("wg-port-" + e.id);
        return {
            engine_id: e.id,
            enabled: en ? !!en.checked : false,
            config_file: file ? file.value : "",
            provider: prov ? prov.value : "",
            manual_port: port ? Number(port.value || 0) : 0,
            // The mode is derived from the provider unless a port was typed in
            // by hand, which is the only case where the operator knows better
            // than the provider table.
            port_forward: (port && Number(port.value || 0) > 0) ? "manual" : "",
        };
    });
    try {
        const r = await api("/api/network/wireguard/engines", {
            method: "POST", headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ engines }),
        });
        out.innerHTML = `<div class="result-msg success">${esc(t(r.note || "Saved."))}</div>`;
        if (r.restart_required) {
            _setRestartBanner(t("Saved. The tunnels come up at the next restart, before the engines start.") +
                ` <button class="btn-small btn-danger" onclick="restartDaemon()" style="margin-left:8px">${t("Apply and restart")}</button>`);
        }
    } catch (e) {
        out.innerHTML = `<div class="result-msg error">${esc(t("Error: {msg}", { msg: e.message }))}</div>`;
    }
}
