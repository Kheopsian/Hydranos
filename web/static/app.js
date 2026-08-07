// Hydra WebUI - Dashboard

const API_KEY = localStorage.getItem("hydra_api_key") || "";
let _provenance = null;
const POLL_INTERVAL = 1000;

let _sessionBaselineUl = null;
let _sessionBaselineDl = null;

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
                warnings.push(`⚠ ${s.ip}:${s.port} — stale socket (bound ${s.bound_interface}, interface recreated)`);
            }
        } else if (!d.all_connectable) {
            cls = d.race_connectable || d.hoard_connectable ? "warn" : "error";
        }

        dot.className = "health-dot " + cls;

        // Build tooltip
        let lines = [
            `Race :${d.race_port} — ${d.race_peers} peers ${d.race_connectable ? "" : "✗"}`,
            `Hoard :${d.hoard_port} — ${d.hoard_peers} peers ${d.hoard_connectable ? "" : "✗"}`,
            `IP: ${d.public_ip}`,
        ];
        if (warnings.length) lines = [...warnings, "", ...lines];
        dot.title = lines.join("\n");
    } catch {}
}

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
            const hdrEl = document.getElementById("header-exit-ip");
            if (hdrEl) hdrEl.textContent = incoExitIP(d.ip);
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
            if (dot) { dot.style.background = ""; dot.title = "Exit IP: " + d.ip + (d.ip_v6 ? " / " + d.ip_v6 : ""); }
            const exitEl = document.getElementById("proxy-exit-ip");
            if (exitEl) exitEl.textContent = incoExitIP(d.ip_v6 || d.ip);
        }
    } catch {}
    finally {
        if (force) {
            stopIpScramble();
            if (rbtn) rbtn.classList.remove("spin");
            if (!ipUpdated && hdrScr) hdrScr.textContent = prevHdr;
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

function _unitsCardHTML() {
    const sz = _unitSize(), sp = _unitSpeed();
    const opt = (v, cur, label) => `<option value="${v}"${v === cur ? " selected" : ""}>${label}</option>`;
    return `<div class="settings-section" style="margin-bottom:18px">
        <div class="settings-section-title">Display units</div>
        <div class="settings-row">
            <div class="sr-label"><span class="sr-key">Sizes</span><span class="sr-desc">Binary (MiB, \u00d71024) or decimal (MB, \u00d71000).</span></div>
            <div class="sr-field"><select class="sr-input" onchange="setUnitPref('size', this.value)">${opt("binary", sz, "Binary \u2014 KiB / MiB / GiB")}${opt("decimal", sz, "Decimal \u2014 KB / MB / GB")}</select></div>
        </div>
        <div class="settings-row">
            <div class="sr-label"><span class="sr-key">Speeds</span><span class="sr-desc">Bytes per second (MiB/s) or bits per second (Mbps).</span></div>
            <div class="sr-field"><select class="sr-input" onchange="setUnitPref('speed', this.value)">${opt("bytes", sp, "Bytes/s \u2014 KiB/s, MiB/s")}${opt("bits", sp, "Bits/s \u2014 Mbps, Gbps")}</select></div>
        </div>
    </div>`;
}

function _interfacesCardHTML(list) {
    const rows = (list && list.length)
        ? list.map((i) => `<div class="settings-row"><div class="sr-label"><span class="sr-key">${esc(i.name)}</span><span class="sr-desc">${i.up ? "up" : "down"}</span></div><div class="sr-field"><code class="sr-readonly">${esc(i.ip)}</code></div></div>`).join("")
        : `<div class="settings-row"><div class="sr-desc">No non-loopback interfaces detected.</div></div>`;
    return `<div class="settings-section" style="margin-bottom:18px">
        <div class="settings-section-title">Network interfaces</div>
        <div class="sr-desc" style="padding:0 0 8px">Detected on this host. To pin an engine to one, set <code>bind_interface</code> to its <b>name</b> (survives VPN IP changes) under [race]/[hoard], or <code>listen_interfaces</code> to <code>ip:port</code>.</div>
        ${rows}
    </div>`;
}

// Throughput in bits/s (network convention, decimal) — Option 2: VOLUMES in iB, RATES in Gbps.
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

let _loginOpen = false;
// Modale de login user/mot de passe. POST /api/login -> renvoie l API key,
// stockee en localStorage (la WebUI l utilise ensuite pour X-Api-Key).
function promptLogin(reason) {
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
                msg.textContent = d.error || ("Login failed (" + res.status + ")");
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
let _importOpen = false;
function importWizard() {
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

    function stepCreds(prefill) {
        box.innerHTML = `<h3>Import from qBittorrent</h3>
            <p class="modal-desc">Point Hydra at your qBittorrent WebUI. Hydra seeds the data already on disk (completed torrents skip the hash-check), so nothing is re-downloaded.</p>
            <input type="text" id="qb-url" placeholder="http://qbittorrent:8080" style="width:100%;margin-bottom:8px" value="${esc(prefill && prefill.url || "")}">
            <input type="text" id="qb-user" placeholder="Username" autocomplete="off" style="width:100%;margin-bottom:8px" value="${esc(prefill && prefill.user || "admin")}">
            <input type="password" id="qb-pass" placeholder="Password" autocomplete="off" style="width:100%">
            <p class="modal-desc" id="qb-msg" style="min-height:1em"></p>
            <div class="modal-actions">
                <button id="qb-skip" class="btn-small">Skip</button>
                <button class="btn-primary" id="qb-preview">Preview</button>
            </div>`;
        box.querySelector("#qb-skip").onclick = () => { localStorage.setItem("hydra_import_dismissed", "1"); close(); };
        const msg = box.querySelector("#qb-msg");
        box.querySelector("#qb-preview").onclick = async () => {
            const creds = {
                url: box.querySelector("#qb-url").value.trim(),
                username: box.querySelector("#qb-user").value,
                password: box.querySelector("#qb-pass").value,
            };
            if (!creds.url) { msg.textContent = "Enter the qBittorrent URL."; return; }
            msg.style.color = ""; msg.textContent = "Connecting…";
            try {
                const res = await fetch("/api/import/qbit/preview", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify(creds),
                });
                const d = await res.json().catch(() => ({}));
                if (!res.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = d.error || ("Preview failed (" + res.status + ")"); return; }
                stepReview(creds, d);
            } catch (e) { msg.style.color = "var(--accent-red)"; msg.textContent = "Network error: " + e.message; }
        };
        box.querySelector("#qb-pass").addEventListener("keydown", e => { if (e.key === "Enter") box.querySelector("#qb-preview").click(); });
    }

    function stepReview(creds, d) {
        const prefixes = d.path_prefixes || [];
        const rows = prefixes.map(p => `<div style="display:flex;gap:6px;align-items:center;margin-bottom:4px">
            <code style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(p)}">${esc(p)}</code>
            <span>→</span>
            <input type="text" class="qb-map" data-from="${esc(p)}" value="${esc(p)}" style="flex:1">
        </div>`).join("");
        box.innerHTML = `<h3>Import preview</h3>
            <p class="modal-desc"><b>${d.total}</b> torrents · <b>${d.completed}</b> complete (seed-mode) · <b>${d.incomplete}</b> partial (verify + resume) · carried upload <b>${formatBytes(d.carried_uploaded_bytes)}</b></p>
            <p class="modal-desc">Categories: ${(d.categories || []).map(c => esc(c.name)).join(", ") || "—"} — all imported as <b>hoard</b>.</p>
            <p class="modal-desc" style="margin-bottom:4px">Path mapping (qBit path → what Hydra sees). Fix these if Hydra mounts the data elsewhere:</p>
            <div style="max-height:160px;overflow:auto;margin-bottom:8px;border:1px solid var(--border);border-radius:4px;padding:6px">${rows || "<i>no paths detected</i>"}</div>
            <p class="modal-desc" id="qb-msg2" style="min-height:1em"></p>
            <div class="modal-actions">
                <button id="qb-back">Back</button>
                <button class="btn-primary" id="qb-go">Import ${d.total} torrents</button>
            </div>`;
        box.querySelector("#qb-back").onclick = () => stepCreds({ url: creds.url, user: creds.username });
        box.querySelector("#qb-go").onclick = async () => {
            const path_map = {};
            box.querySelectorAll(".qb-map").forEach(inp => {
                const f = inp.dataset.from, t = inp.value.trim();
                if (t && t !== f) path_map[f] = t;
            });
            const msg = box.querySelector("#qb-msg2");
            msg.style.color = ""; msg.textContent = "Starting…";
            try {
                const res = await fetch("/api/import/qbit/start", {
                    method: "POST",
                    headers: { "Content-Type": "application/json", "X-Api-Key": API_KEY },
                    body: JSON.stringify(Object.assign({}, creds, { path_map })),
                });
                const d2 = await res.json().catch(() => ({}));
                if (!res.ok) { msg.style.color = "var(--accent-red)"; msg.textContent = d2.error || ("Start failed (" + res.status + ")"); return; }
                stepProgress();
            } catch (e) { msg.style.color = "var(--accent-red)"; msg.textContent = "Network error: " + e.message; }
        };
    }

    function stepProgress() {
        box.innerHTML = `<h3>Importing…</h3>
            <p class="modal-desc" id="qb-phase">Connecting…</p>
            <div style="background:var(--border);border-radius:4px;height:10px;overflow:hidden;margin:8px 0">
                <div id="qb-bar" style="height:100%;width:0;background:var(--accent-hoard);transition:width .2s"></div>
            </div>
            <p class="modal-desc" id="qb-stats"></p>
            <p class="modal-desc" id="qb-cur" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;opacity:.7"></p>
            <div class="modal-actions"><button class="btn-primary" id="qb-done" style="display:none">Close &amp; reload</button></div>`;
        const q = API_KEY ? ("?apikey=" + encodeURIComponent(API_KEY)) : "";
        const es = new EventSource("/api/import/qbit/events" + q);
        const phase = box.querySelector("#qb-phase"), bar = box.querySelector("#qb-bar"),
            stats = box.querySelector("#qb-stats"), cur = box.querySelector("#qb-cur"),
            doneBtn = box.querySelector("#qb-done");
        const labels = { connect: "Connecting to qBittorrent…", categories: "Creating categories…", torrents: "Importing torrents…", done: "Done", error: "Error" };
        es.onmessage = (ev) => {
            let d; try { d = JSON.parse(ev.data); } catch (e) { return; }
            phase.style.color = ""; phase.textContent = labels[d.phase] || d.phase;
            if (d.total > 0) bar.style.width = Math.round(100 * d.done / d.total) + "%";
            stats.textContent = `${d.done}/${d.total} · ${d.seeded} seeding · ${d.downloading} resuming · ${d.failed} failed${d.skipped ? " \u00b7 " + d.skipped + " skipped" : ""}`;
            cur.textContent = d.current ? ("last: " + d.current) : "";
            if (d.phase === "error") { phase.style.color = "var(--accent-red)"; phase.textContent = "Error: " + (d.error || "unknown"); }
            if (d.finished) {
                es.close();
                if (d.phase !== "error") { bar.style.width = "100%"; phase.textContent = "Import complete"; }
                doneBtn.style.display = "";
                doneBtn.onclick = () => location.reload();
            }
        };
        es.onerror = () => { /* EventSource auto-retries; a finished job's stream 404s and stops */ };
    }

    stepCreds();
}
window.importFromQbit = importWizard;

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
        promptLogin("Session invalid \u2014 please sign in again.");
        throw new Error("API error: 401");
    }
    if (!res.ok) throw new Error(`API error: ${res.status}`);
    return res.json();
}

// ─── Tabs ───────────────────────────────────────────────

function activateTab(name) {
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
    else if (name === "changelog") loadChangelog();
    else if (name === "agents") { updateAgents(); updateEngines(); }
    else if (name === "trackers") { updateTrackers(); loadTrackerStats(); }
    else if (name === "logs") loadLogs();
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
        else if (_hashTab === "agents") { await updateAgents(); updateEngines(); }
        else if (_hashTab === "trackers") { await updateTrackers(); loadTrackerStats(); }
        else if (_hashTab === "benchmark") await updateBenchmark();
    });
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
// status_snapshot handler. Pure render — does no I/O.
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

        // Day totals — UL/DL accumulated since midnight Europe/Paris (auto reset)
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

            // Compteur milestone PiB DYNAMIQUE (prochain jalon entier) — 1 PiB = 2^50 octets
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
            if (tagEl) tagEl.innerHTML = "\u25C6 " + goal + " PiB milestone";
            const fillEl = document.getElementById("ov-milestone-fill");
            if (fillEl) fillEl.style.width = Math.min(100, (pib - (goal - 1)) * 100).toFixed(1) + "%";
            const togoEl = document.getElementById("ov-milestone-togo");
            if (togoEl) {
                const togoTiB = (goal - pib) * 1024;
                togoEl.textContent = togoTiB.toFixed(1) + " TiB to go";
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

        // Uptime — moved here from /metrics fetch (now sourced from status payload).
        if (typeof data.uptime === "number") {
            const upEl = document.getElementById("uptime-value");
            if (upEl) upEl.textContent = formatUptime(data.uptime);
        }

        document.getElementById("last-update").textContent =
            "Last update: " + new Date().toLocaleTimeString();
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
        const label = n => (ord[n - 1] || (n + "th")) + " pebibyte";
        const rows = [];
        if (_provenance && _provenance.present) {
            const since = _provenance.source_date ? new Date(_provenance.source_date * 1000).toLocaleDateString("en-US", { year: "numeric", month: "short" }) : "";
            rows.push('<div class="lead">same counter &middot; carried over from ' + esc(_provenance.source_client || "a previous client") + (since ? (' &middot; since ' + since) : '') + '</div>');
        }
        d.milestones.forEach((m, i) => {
            if (!m.observed) {
                rows.push('<div class="ar"><span class="when">' + label(m.pib) +
                    '</span><span class="via">libtorrent (C++) &middot; qBittorrent</span><span class="dur">pre-Hydra</span></div>');
            } else {
                const hot = (i === d.milestones.length - 1) ? " hot" : "";
                const dur = m.since_prev || "&mdash;";
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
let detailRefreshTimer = null;
let selectedHoardTorrent = null;
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
            document.getElementById("race-avg-share").textContent = "—";
        }
    } catch (e) {
        console.error("Failed to update race torrents:", e);
    }
}

async function showDetail(infoHash) {
    selectedTorrent = infoHash;
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
    document.querySelectorAll(".detail-tab").forEach(btn =>
        btn.classList.toggle("active", btn.textContent.toLowerCase() === tab)
    );
    document.getElementById("detail-tab-info").style.display = tab === "info" ? "" : "none";
    document.getElementById("detail-tab-timeline").style.display = tab === "timeline" ? "" : "none";
    if (tab === "timeline" && selectedTorrent) loadTimeline(selectedTorrent);
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
        // must not swallow the rest — notably the peers list and event log.
        const safe = (fn, label) => { try { fn(); } catch (e) { console.error("timeline render failed:", label, e); } };

        safe(() => renderTimelineResult(events, snapshots, t0), "result");
        safe(() => renderTimelineMeta(events, snapshots, t0), "meta");
        if (typeof Chart !== "undefined") {
            safe(() => renderProgressChart(snapshots, events, t0), "progress-chart");
            safe(() => renderPeerSpeedChart(snapshots, t0), "peer-chart");
        } else {
            console.warn("Chart.js unavailable — skipping timeline graphs");
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

    // Find competitors — exclude initial seeders (already at 100% in first snapshot)
    const initialSeeders = new Set();
    const seenAsLeecher = new Set();  // peers we saw with progress < 1.0
    let firstSnapParsed = false;
    for (const snap of snapshots) {
        if (!snap.peers_json || snap.peers_json === "[]") continue;
        try {
            const peers = JSON.parse(snap.peers_json);
            if (!firstSnapParsed) {
                // First snapshot with peers — mark initial seeders
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
            <div class="tl-pos" style="color:${won ? '#3fb950' : '#f85149'}">${position}${position===1?'st':position===2?'nd':position===3?'rd':'th'} <span>/ ${total}</span></div>
        </div>
        <div class="tl-detail">
            ${!won ? `<div style="color:#f85149;font-weight:600">Lost by ${formatDuration(dlTime - winnerTime)}</div>` : '<div style="color:#3fb950;font-weight:600">Winner!</div>'}
            ${!won && winnerTime > 0 ? `<div style="color:var(--text-muted)">Winner ${(dlTime/winnerTime).toFixed(1)}× faster</div>` : ''}
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
        <div class="tl-meta-item"><div class="tl-meta-label">Size</div><div class="tl-meta-val">${formatBytes(size)}</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">Duration</div><div class="tl-meta-val">${formatDuration(dlTime)}</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">Avg DL</div><div class="tl-meta-val">${formatBytes(avgDL)}/s</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">Peak DL</div><div class="tl-meta-val good">${formatBytes(peakDL)}/s</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">Ratio</div><div class="tl-meta-val">${finalRatio.toFixed(2)}</div></div>
        <div class="tl-meta-item"><div class="tl-meta-label">Peers max</div><div class="tl-meta-val">${Math.max(...snapshots.map(s => s.peers || 0))}</div></div>`;
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
            label: "DL Rate", data: ourDLRate,
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
                            return `${ctx.dataset.label}: ${ctx.raw !== null ? ctx.raw.toFixed(1) + "%" : "—"}`;
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
        if (ev.event === "uploader_injected") detail = `${ev.uploader} (${ev.injected_peers} peers)`;
        if (ev.event === "announce") detail = `${ev.swarm_seeds}s/${ev.swarm_leechers}l`;
        return `<div class="tl-ev-row${isHl ? " hl" : ""}">
            <span class="tl-ev-t">${tPlus}</span>
            <span style="min-width:14px;text-align:center;font-size:10px">${icon}</span>
            <span class="tl-ev-txt"><strong>${ev.event.replace(/_/g," ")}</strong> <span class="d">${detail}</span></span>
        </div>`;
    }).join("");
}

function formatDate(ts) {
    if (!ts || ts <= 0) return "—";
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
        const d = await api(`/api/race/torrents/${selectedTorrent}`);

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

        // Trackers table
        const ttbody = document.getElementById("detail-trackers-tbody");
        if (d.trackers && d.trackers.length > 0) {
            ttbody.innerHTML = d.trackers.map(t => {
                let domain = t.url;
                try { domain = new URL(t.url).hostname; } catch (_) {}
                const ep = t.endpoints && t.endpoints[0];
                const hasErr = ep && ep.last_error && ep.last_error !== "Success";
                const msg = ep ? (ep.message || ep.last_error || "") : "";
                const nextAnn = ep ? ep.next_announce : 0;
                const seeds = ep ? ep.scrape_complete : -1;
                const leechers = ep ? ep.scrape_incomplete : -1;
                const statusHtml = hasErr
                    ? `<span class="tracker-err" title="${msg}">${msg.substring(0, 60) || "error"}</span>`
                    : `<span class="tracker-ok">${msg || "OK"}</span>`;
                const nextStr = nextAnn > 0 ? `${Math.floor(nextAnn/60)}m${nextAnn%60}s` : nextAnn === 0 ? "now" : "-";
                const scrapeStr = seeds >= 0 ? `${seeds}s/${leechers}l` : "";
                return `<tr><td class="mono" title="${t.url}">${domain}</td><td>${statusHtml}</td><td class="mono">${nextStr}</td><td class="mono">${scrapeStr}</td></tr>`;
            }).join("");
        } else {
            ttbody.innerHTML = '<tr><td colspan="4" class="empty">No trackers</td></tr>';
        }
    } catch (e) {
        console.error("Failed to refresh detail:", e);
    }
}

// ─── Hoard stats ────────────────────────────────────────

function setHoardStateFilter(el, value) {
    _hoardStateFilter = value;
    document.querySelectorAll(".chip-state").forEach(c => c.classList.toggle("active", c === el));
    renderHoardTable();
}

function setHoardCatFilter(el, value) {
    _hoardCatFilter = _hoardCatFilter === value ? "" : value;
    document.querySelectorAll(".chip-cat").forEach(c => c.classList.toggle("active", c.dataset.cat === _hoardCatFilter && _hoardCatFilter !== ""));
    renderHoardTable();
}

function setHoardTrackerFilter(el, value) {
    _hoardTrackerFilter = _hoardTrackerFilter === value ? "" : value;
    document.querySelectorAll(".chip-tracker").forEach(c => c.classList.toggle("active", c.dataset.tracker === _hoardTrackerFilter && _hoardTrackerFilter !== ""));
    renderHoardTable();
}

function setHoardTagFilter(el, value) {
    _hoardTagFilter = _hoardTagFilter === value ? "" : value;
    document.querySelectorAll(".chip-tag").forEach(c => c.classList.toggle("active", c.dataset.tag === _hoardTagFilter && _hoardTagFilter !== ""));
    renderHoardTable();
}

// --- Tags context-menu editor (hoard-only, multi-select) ---
async function _showTagPicker(ev) {
    if (ev) ev.stopPropagation();
    _saveCtxActionsView();
    const hoardOnly = [..._selected.entries()].filter(([, m]) => m === "hoard");
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
    const label = `Edit tags — ${hoardOnly.length} torrent${hoardOnly.length > 1 ? "s" : ""}`;
    document.getElementById("ctx-menu").innerHTML =
        `<div class="ctx-label">${label}</div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-item" onclick="_restoreCtxActionsView()">&lsaquo; Retour</div>` +
        `<div class="ctx-separator"></div>` +
        `<div style="padding:6px 10px"><input type="text" id="ctx-new-tag" placeholder="new tag + Enter" style="width:100%" onclick="event.stopPropagation()" onkeydown="if(event.key==='Enter'){event.preventDefault();_addNewTagSelected();}"></div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-scroll">${rows || '<div class="ctx-item" style="opacity:.6">No tags yet</div>'}</div>`;
    _clampCtxMenuToViewport();
}

async function _applyTagOp(tags, op) {
    const entries = [..._selected.entries()].filter(([, m]) => m === "hoard");
    for (const [hash] of entries) {
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

    let filtered = _hoardAllTorrents;
    if (search) filtered = filtered.filter(t => (t.name || t.info_hash).toLowerCase().includes(search));
    if (catFilter === "__none__") filtered = filtered.filter(t => !(t.category));
    else if (catFilter) filtered = filtered.filter(t => (t.category || "") === catFilter);
    if (_hoardTrackerFilter) filtered = filtered.filter(t => (t.tracker_host || "") === _hoardTrackerFilter);
    if (_hoardTagFilter === "__none__") filtered = filtered.filter(t => !(t.tags && t.tags.length));
    else if (_hoardTagFilter) filtered = filtered.filter(t => (t.tags || []).includes(_hoardTagFilter));
    if (stateFilter === "__active__") filtered = filtered.filter(t => t.state === "seeding" && t.upload_rate > 0);
    else if (stateFilter === "__tracker_err__") filtered = filtered.filter(t => t.tracker_error);
    else if (stateFilter === "__error__") filtered = filtered.filter(t => t.torrent_error);
    else if (stateFilter) filtered = filtered.filter(t => t.state === stateFilter);

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

    const visible = filtered.slice(0, HOARD_RENDER_LIMIT);
    const countEl = document.getElementById("hoard-filter-count");
    if (countEl) {
        if (filtered.length > HOARD_RENDER_LIMIT)
            countEl.textContent = `${HOARD_RENDER_LIMIT} / ${filtered.length} (${_hoardAllTorrents.length} total)`;
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
// (30s backstop fetch, torrent_added/removed) — not on each stats snapshot.
function _renderHoardCounts() {
    if (!_hoardAllTorrents) return;
    const stateCounts = {};
    _hoardAllTorrents.forEach(t => { stateCounts[t.state] = (stateCounts[t.state] || 0) + 1; });
    const nAll = _hoardAllTorrents.length;
    const nActive = _hoardAllTorrents.filter(t => t.state === "seeding" && t.upload_rate > 0).length;
    const nTrackerErr = _hoardAllTorrents.filter(t => t.tracker_error).length;
    const nTorrentErr = _hoardAllTorrents.filter(t => t.torrent_error).length;
    document.querySelector(".chip-state[data-state='']").innerHTML = `All <span class="chip-count">${nAll}</span>`;
    document.querySelector(".chip-state[data-state='seeding']").innerHTML = `Seeding <span class="chip-count">${stateCounts["seeding"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='__active__']").innerHTML = `Actively Seeding <span class="chip-count">${nActive}</span>`;
    document.querySelector(".chip-state[data-state='downloading']").innerHTML = `Downloading <span class="chip-count">${stateCounts["downloading"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='paused']").innerHTML = `Parked <span class="chip-count">${stateCounts["paused"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='checking_files']").innerHTML = `Checking <span class="chip-count">${stateCounts["checking_files"] || 0}</span>`;
    document.querySelector(".chip-state[data-state='__tracker_err__']").innerHTML = `Tracker Error <span class="chip-count">${nTrackerErr}</span>`;
    document.querySelector(".chip-state[data-state='__error__']").innerHTML = `Error <span class="chip-count">${nTorrentErr}</span>`;

    const catCounts = {};
    _hoardAllTorrents.forEach(t => {
        const c = t.category || "";
        if (c) catCounts[c] = (catCounts[c] || 0) + 1;
    });
    const cats = Object.keys(catCounts).sort();
    const nUncat = _hoardAllTorrents.filter(t => !(t.category)).length;
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
    _hoardAllTorrents.forEach(t => { const h = t.tracker_host || ""; if (h) trkCounts[h] = (trkCounts[h] || 0) + 1; });
    const trks = Object.keys(trkCounts).sort();
    const trkContainer = document.getElementById("hoard-tracker-chips");
    if (trkContainer) {
        trkContainer.innerHTML = trks.map(h =>
            `<button class="chip chip-tracker${h === _hoardTrackerFilter ? " active" : ""}" data-tracker="${esc(h)}" onclick="setHoardTrackerFilter(this,'${h}')">${esc(h)} <span class="chip-count">${trkCounts[h]}</span></button>`
        ).join("");
    }

    const tagCounts = {};
    _hoardAllTorrents.forEach(t => { (t.tags || []).forEach(tg => { tagCounts[tg] = (tagCounts[tg] || 0) + 1; }); });
    const tagNames = Object.keys(tagCounts).sort();
    const nUntagged = _hoardAllTorrents.filter(t => !(t.tags && t.tags.length)).length;
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
        const annText = annPct >= 100 ? "all announced" : `${announced}/${data.total_torrents} announced (${annPct}%)`;

        // Peer efficiency: connected peers vs available swarm leechers
        const swarm = data.swarm_leechers || 0;
        const peers = data.unseeded_peers ?? data.active_peers ?? 0;
        let peerText = `${peers} peers`;
        if (swarm > 0) {
            const peerPct = (peers / swarm * 100).toFixed(1);
            peerText = `${peers}/${formatCount(swarm)} peers (${peerPct}%)`;
        }

    document.getElementById("hoard-summary-text").textContent =
        `${data.total_torrents} torrents \u2014 ${data.torrents_uploading} uploading, ${data.torrents_with_peers} with peers, ${peerText} \u2014 ${annText}`;
}

// Called on tab activation, hash change, or page load. Live updates flow
// through SSE (status_snapshot + hoard_stats_snapshot + stats_snapshot) so
// the periodic poll does NOT call this anymore — it would duplicate the SSE
// stream over HTTP, which was the original bug we set out to kill.
//
// Two responsibilities:
//   1. Fire an immediate /api/hoard/stats fetch only if SSE hasn't painted
//      the header yet (cold tab activation).
//   2. Refresh the static-ish torrent list (/api/hoard/torrents) at most
//      every HOARD_FETCH_INTERVAL — backstop for name / category / scrape.
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

async function showHoardDetail(infoHash) {
    selectedHoardTorrent = infoHash;
    document.getElementById("hoard-detail-panel").style.display = "block";
    if (hoardDetailTimer) clearInterval(hoardDetailTimer);
    await refreshHoardDetail();
    hoardDetailTimer = setInterval(refreshHoardDetail, 3000);
}

function closeHoardDetail() {
    selectedHoardTorrent = null;
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

const _selected = new Map(); // hash → mode
let _anchorHash = null;      // last click without shift (range start point)

function handleRowClick(e, hash, mode) {
    if (e.shiftKey && _anchorHash) {
        // Range selection: all .t-row between the anchor and the clicked row
        const rows = [...document.querySelectorAll(".t-row")];
        const aIdx = rows.findIndex(r => r.dataset.hash === _anchorHash);
        const bIdx = rows.findIndex(r => r.dataset.hash === hash);
        if (aIdx !== -1 && bIdx !== -1) {
            const lo = Math.min(aIdx, bIdx);
            const hi = Math.max(aIdx, bIdx);
            // Keep only the range selection (like Windows Explorer)
            _selected.clear();
            for (let i = lo; i <= hi; i++) {
                _selected.set(rows[i].dataset.hash, rows[i].dataset.mode);
            }
        }
        _updateRowHighlights();
        return;
    }
    if (e.ctrlKey || e.metaKey) {
        if (_selected.has(hash)) _selected.delete(hash);
        else _selected.set(hash, mode);
        _anchorHash = hash;
        _updateRowHighlights();
        return;
    }
    _selected.clear();
    _selected.set(hash, mode);
    _anchorHash = hash;
    _updateRowHighlights();
    requestAnimationFrame(() => {
        if (mode === "race") showDetail(hash);
        else showHoardDetail(hash);
    });
}

function handleRowContextMenu(e, hash, mode) {
    e.preventDefault();
    if (!_selected.has(hash)) {
        _selected.clear();
        _selected.set(hash, mode);
        _updateRowHighlights();
    }
    _showCtxMenu(e.clientX, e.clientY);
}

function _updateRowHighlights() {
    document.querySelectorAll(".t-row").forEach(row => {
        row.classList.toggle("row-selected", _selected.has(row.dataset.hash));
    });
}

function _showCtxMenu(x, y) {
    const menu = document.getElementById("ctx-menu");
    const count = _selected.size;
    document.getElementById("ctx-label").textContent =
        count > 1 ? `${count} torrents selected` : "1 torrent selected";
    // "Change category" only applies to hoard torrents. Hide the item if
    // the current selection has no hoard rows.
    const anyHoard = [..._selected.entries()].some(([, m]) => m === "hoard");
    const catItem = document.getElementById("ctx-change-category");
    if (catItem) catItem.style.display = anyHoard ? "" : "none";
    const rcItem = document.getElementById("ctx-recheck");
    if (rcItem) rcItem.style.display = anyHoard ? "" : "none";
    const tgItem = document.getElementById("ctx-edit-tags");
    if (tgItem) tgItem.style.display = anyHoard ? "" : "none";
    // Offer whichever of Pause/Resume the selection can actually act on. A
    // mixed selection gets both.
    const intents = [..._selected.keys()].map(_isUserPaused);
    const anyPaused = intents.some(v => v === true || v === null);
    const anyRunning = intents.some(v => v === false || v === null);
    const pItem = document.getElementById("ctx-pause");
    if (pItem) pItem.style.display = anyRunning ? "" : "none";
    const rItem = document.getElementById("ctx-resume");
    if (rItem) rItem.style.display = anyPaused ? "" : "none";
    menu.style.left = x + "px";
    menu.style.top = y + "px";
    menu.style.display = "block";
    _clampCtxMenuToViewport();
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
    _restoreCtxActionsView();
}

// Cached HTML of the default ctx-menu (actions view) so we can restore it
// after navigating into the category sub-picker.
let _ctxActionsHTML = null;

function _saveCtxActionsView() {
    if (_ctxActionsHTML === null) {
        _ctxActionsHTML = document.getElementById("ctx-menu").innerHTML;
    }
}

function _restoreCtxActionsView() {
    if (_ctxActionsHTML !== null) {
        document.getElementById("ctx-menu").innerHTML = _ctxActionsHTML;
        _clampCtxMenuToViewport();
    }
}

async function _showCategoryPicker(ev, move) {
    if (ev) ev.stopPropagation();
    _saveCtxActionsView();
    const hoardOnly = [..._selected.entries()].filter(([, m]) => m === "hoard");
    if (hoardOnly.length === 0) return;
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
        // category name is embedded as a JS string literal — escape quotes.
        const jsName = String(c.name).replace(/\\/g, "\\\\").replace(/\'/g, "\\\'");
        return `<div class="ctx-item" onclick="_changeCategorySelected(\'${jsName}\', ${move ? "true" : "false"})" title="${safePath}">${safeName}</div>`;
    }).join("");
    const verb = move ? "Move to category" : "Set category (no move)";
    const label = `${verb} \u2014 ${hoardOnly.length} torrent${hoardOnly.length > 1 ? "s" : ""}`;
    document.getElementById("ctx-menu").innerHTML =
        `<div class="ctx-label">${label}</div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-item" onclick="_restoreCtxActionsView()">&lsaquo; Retour</div>` +
        `<div class="ctx-separator"></div>` +
        `<div class="ctx-scroll">${items}</div>`;
    _clampCtxMenuToViewport();
}

async function _changeCategorySelected(catName, move) {
    const entries = [..._selected.entries()].filter(([, m]) => m === "hoard");
    _hideCtxMenu();
    let okCount = 0;
    const errors = [];
    for (const [hash] of entries) {
        try {
            const r = await fetch(`/api/hoard/torrents/${hash}/category`, {
                method: "POST",
                headers: {
                    "X-Api-Key": API_KEY,
                    "Content-Type": "application/json",
                },
                body: JSON.stringify({ category: catName, move_files: !!move }),
            });
            if (!r.ok) {
                let msg = `HTTP ${r.status}`;
                try {
                    const j = await r.json();
                    if (j && j.error) msg = j.error;
                } catch (_) { /* ignore */ }
                errors.push(`${hash.slice(0, 8)}: ${msg}`);
            } else {
                okCount++;
            }
        } catch (err) {
            errors.push(`${hash.slice(0, 8)}: ${err.message}`);
        }
    }
    if (errors.length > 0) {
        alert(`Category changed to "${catName}": ${okCount} OK, ${errors.length} failure(s).\n\n${errors.join("\n")}`);
    }
    // Optimistic: reflect the new category in the held rows immediately, so it
    // shows without waiting for the next full refresh.
    if (Array.isArray(_hoardAllTorrents)) {
        for (const [hash] of entries) {
            const t = _hoardAllTorrents.find(x => x.info_hash === hash);
            if (t) t.category = catName;
        }
    }
    try { if (typeof _scheduleHoardRender === "function") _scheduleHoardRender(); } catch (_) {}
}

document.addEventListener("click", e => {
    if (!e.target.closest("#ctx-menu")) _hideCtxMenu();
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
    for (const [hash] of entries) {
        try {
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
// What the row should say. "Paused" is the user's own decision; "Queued" is a
// scheduler holding the torrent back, which is normal and temporary. Keeping
// them apart is the whole point of the pause intent — same split qBittorrent
// makes, so the words already mean what people expect.
function displayState(t) {
    if (t.user_paused) {
        return { label: "paused", cls: "state-paused", title: "Paused by you — survives a restart" };
    }
    if (t.state === "stopped" || t.state === "queued") {
        return { label: "queued", cls: "state-queued", title: "Waiting for a slot" };
    }
    return {
        label: t.state,
        cls: t.state === "seeding" ? "state-active" : "state-checking",
        title: t.state,
    };
}

function _isUserPaused(hash) {
    const t = _hoardAllTorrents.find(x => x.info_hash === hash);
    return t ? !!t.user_paused : null;
}

// Pause or resume the selection. This writes the user's intent: it outlives a
// restart and no scheduler will undo it.
async function _pauseSelected(paused) {
    _hideCtxMenu();
    const byEngine = { hoard: [], race: [] };
    for (const [hash, mode] of _selected.entries()) {
        (byEngine[mode] || byEngine.hoard).push(hash);
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
            console.error(`Failed to ${paused ? "pause" : "resume"} ${engine}`, err);
        }
    }
    updateHoardStats();
}

async function _recheckSelected() {
    _hideCtxMenu();
    const entries = [..._selected.entries()];
    for (const [hash, mode] of entries) {
        if (mode !== "hoard") continue;
        try {
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
    for (const [hash] of entries) {
        try {
            await fetch(`/api/torrents/${hash}?delete_files=${deleteFiles}`, {
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
        const d = await api(`/api/hoard/torrents/${selectedHoardTorrent}`);

        document.getElementById("h-detail-name").textContent = incoName(d);
        document.getElementById("h-detail-state").textContent = d.state;
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

        const ttbody = document.getElementById("h-detail-trackers-tbody");
        if (d.trackers && d.trackers.length > 0) {
            ttbody.innerHTML = d.trackers.map(t => {
                let domain = t.url;
                try { domain = new URL(t.url).hostname; } catch (_) {}
                const ep = t.endpoints && t.endpoints[0];
                const hasErr = ep && ep.last_error && ep.last_error !== "Success";
                const msg = ep ? (ep.message || ep.last_error || "") : "";
                const nextAnn = ep ? ep.next_announce : 0;
                const seeds = ep ? ep.scrape_complete : -1;
                const leechers = ep ? ep.scrape_incomplete : -1;
                const statusHtml = hasErr
                    ? `<span class="tracker-err" title="${msg}">${msg.substring(0, 60) || "error"}</span>`
                    : `<span class="tracker-ok">${msg || "OK"}</span>`;
                const nextStr = nextAnn > 0 ? `${Math.floor(nextAnn/60)}m${nextAnn%60}s` : nextAnn === 0 ? "now" : "-";
                const scrapeStr = seeds >= 0 ? `${seeds}s/${leechers}l` : "";
                return `<tr><td class="mono" title="${t.url}">${domain}</td><td>${statusHtml}</td><td class="mono">${nextStr}</td><td class="mono">${scrapeStr}</td></tr>`;
            }).join("");
        } else {
            ttbody.innerHTML = '<tr><td colspan="4" class="empty">No trackers</td></tr>';
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
        `${speedOk ? "" : "✗"} Avg speed > 10 MB/s (${formatSpeed(p.avg_speed)})`,
        `${reliabilityOk ? "" : "✗"} Reliability > 80% (${(p.reliability * 100).toFixed(0)}%)`,
        `${sessionsOk ? "" : "✗"} Sessions > 10 (${p.num_sessions})`,
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
        alert("Failed to remove: " + e.message);
    }
}

// ─── Add torrent ────────────────────────────────────────

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
    btn.textContent = "Adding\u2026";
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
                    resultEl.textContent = `Upload ${i + 1}/${files.length} — ${files[i].name}`;
                    resultEl.className = "result-msg";
                    resultEl.style.display = "block";
                    try {
                        const formData = new FormData();
                        formData.append("file", files[i]);
                        formData.append("mode", mode);
                        formData.append("save_path", savePath);
                        if (category) formData.append("category", category);
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
                    resultEl.textContent = `${ok} torrent${ok > 1 ? "s" : ""} added`;
                    resultEl.className = "result-msg success";
                } else {
                    resultEl.innerHTML = `${ok} OK, ${fail} erreur${fail > 1 ? "s" : ""} :<br>` +
                        errors.map(e => `<small>${e}</small>`).join("<br>");
                    resultEl.className = "result-msg " + (ok > 0 ? "" : "error");
                }
                resultEl.style.display = "block";
                clearTimeout(_addMsgTimer);
                if (fail === 0) _addMsgTimer = setTimeout(() => { resultEl.style.display = "none"; }, 6000);
                fileInput.value = "";
                btn.textContent = btn.dataset.label || "Add Torrent";
                btn.disabled = false;
                updateOverview(); updateRaceTorrents(); updateHoardStats();
                return;
            }
        } else {
            // Path or magnet mode
            const body = {
                mode: mode,
                save_path: document.getElementById("save-path").value,
                category: category || undefined,
            };

            const torrentPath = document.getElementById("torrent-path").value.trim();
            const magnetUri = document.getElementById("magnet-uri").value.trim();

            if (torrentPath) {
                body.torrent_path = torrentPath;
            } else if (magnetUri && mode === "race") {
                body.magnet_uri = magnetUri;
            } else {
                throw new Error("Provide a torrent path, magnet URI, or upload a file");
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
        setTimeout(() => { btn.textContent = btn.dataset.label || "Add Torrent"; }, 1500);

        // Reset form
        document.getElementById("torrent-path").value = "";
        document.getElementById("magnet-uri").value = "";
        fileInput.value = "";

        // Refresh lists
        updateOverview();
        updateRaceTorrents();
        updateHoardStats();
    } catch (err) {
        resultEl.textContent = "Error: " + err.message;
        resultEl.className = "result-msg error";
        resultEl.style.display = "";
        btn.textContent = btn.dataset.label || "Add Torrent";
    } finally {
        btn.disabled = false;
    }
});

// ─── Categories ─────────────────────────────────────────

let _editingCategory = null;

async function updateCategories() {
    try {
        const cats = await api("/api/categories");
        const tbody = document.getElementById("categories-tbody");

        // Update table
        if (!cats || cats.length === 0) {
            tbody.innerHTML = '<tr><td colspan="7" class="empty">No categories</td></tr>';
        } else {
            tbody.innerHTML = cats.map(cat => `<tr>
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
        }

        // Labels worn by torrents but matching no configured category: the
        // residue of deletions made before labels were cleared durably. Shown
        // here because this screen holds the only delete button, and left out
        // of the dropdown below — you cannot assign a category that is gone.
        try {
            const orphans = await api("/api/categories/orphans");
            if (orphans && orphans.length) {
                if (!cats || cats.length === 0) tbody.innerHTML = "";
                tbody.innerHTML += orphans.map(o => `<tr>
                    <td><strong>${esc(incoCat(o.name))}</strong> <span style="color:var(--text-muted);font-size:11px">orphaned</span></td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td class="mono" style="font-size:12px;color:var(--text-muted)">no longer configured, still on ${o.torrents} torrent${o.torrents > 1 ? "s" : ""}</td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td><span style="color:var(--text-muted)">\u2014</span></td>
                    <td><button class="btn-small btn-danger" onclick="deleteCategory('${o.name}')">Delete</button></td>
                </tr>`).join("");
            }
        } catch (e) { console.error("load orphaned categories", e); }

        // Update add-torrent dropdown
        const sel = document.getElementById("torrent-category");
        const current = sel.value;
        sel.innerHTML = '<option value="">— none —</option>' +
            cats.map(cat => `<option value="${cat.name}"${cat.name === current ? " selected" : ""}>${esc(incoCat(cat.name))}</option>`).join("");
    } catch (e) {
        console.error("Failed to update categories:", e);
    }
}

async function showCategoryForm(name = null) {
    _editingCategory = name;
    document.getElementById("cat-name").value = name || "";
    document.getElementById("cat-mode").value = "race";
    document.getElementById("cat-save-path").value = "";
    document.getElementById("cat-strategy").value = "all";
    document.getElementById("cat-result").style.display = "none";
    document.getElementById("category-form").style.display = "block";

    let allCats = [];
    try { allCats = (await api("/api/categories")) || []; } catch (e) { console.error("load categories", e); }
    let cat = name ? (allCats.find(c => c.name === name) || null) : null;
    if (cat) {
        document.getElementById("cat-mode").value = cat.mode;
        document.getElementById("cat-save-path").value = cat.save_path;
        document.getElementById("cat-strategy").value = cat.strategy || "all";
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
        ? '<div class="fs-dir-item fs-empty">— no folders —</div>'
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

async function saveCategory() {
    const name = document.getElementById("cat-name").value.trim();
    const mode = document.getElementById("cat-mode").value;
    const save_path = document.getElementById("cat-save-path").value.trim();
    const resultEl = document.getElementById("cat-result");

    if (!name || !save_path) {
        resultEl.textContent = "Name and save path are required";
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
        const graduate_to = mode === "race" ? (document.getElementById("cat-graduate-to").value || "") : "";
        const payload = { name, save_path, mode, placement, agents, strategy, graduate_to };
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
        resultEl.textContent = "Error: " + e.message;
        resultEl.className = "result-msg error";
        resultEl.style.display = "block";
    }
}

async function deleteCategory(name) {
    if (!confirm(`Delete category "${name}"?`)) return;
    try {
        await api(`/api/categories/${encodeURIComponent(name)}`, { method: "DELETE" });
        await updateCategories();
    } catch (e) {
        alert("Error: " + e.message);
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
    race_uploading:          { label: "Upload actif Race",   fmt: v => v.toFixed(0),         hb: true  },
    hoard_upload_rate:       { label: "Upload Hoard",         fmt: formatSpeed,               hb: true  },
    hoard_peers:             { label: "Peers Hoard",          fmt: v => v.toFixed(0),         hb: true  },
    hoard_active:            { label: "Actifs Hoard",         fmt: v => v.toFixed(0),         hb: true  },
    hoard_with_peers:        { label: "Avec Peers Hoard",     fmt: v => v.toFixed(0),         hb: true  },
    hoard_uploading:         { label: "Upload actif Hoard",   fmt: v => v.toFixed(0),         hb: true  },
    iowait_pct:              { label: "IOWait %",             fmt: v => v.toFixed(2) + "%",   hb: false },
    arc_size_bytes:          { label: "ARC Size",             fmt: formatBytes,               hb: null  },
    arc_hit_rate_pct:        { label: "ARC Hit Rate",         fmt: v => v.toFixed(3) + "%",   hb: true  },
    arc_demand_hit_rate_pct: { label: "ARC Demand Hit Rate",  fmt: v => v.toFixed(3) + "%",   hb: true  },
    arc_miss_per_sec:        { label: "ARC Miss/s",           fmt: v => v.toFixed(1),         hb: false },
    arc_demand_miss_per_sec: { label: "ARC Demand Miss/s",    fmt: v => v.toFixed(1),         hb: false },
    arc_ghost_hits_per_sec:  { label: "ARC Ghost Hits/s",     fmt: v => v.toFixed(1),         hb: null  },
};

// Crosshair plugin — vertical line on hover
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
// covers 7 days at hourly cadence — hour-format makes the X axis unreadable).
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

function _updateDualChart(chart, history, key1, key2) {
    chart.data.labels = history.map(p => _bmLabel(p.ts));
    chart.data.datasets[0].data = history.map(p => p[key1] ?? 0);
    chart.data.datasets[1].data = history.map(p => p[key2] ?? 0);
    chart.update("none");
}

function _initBmCharts() {
    Chart.defaults.color = "#7a90a8";
    _bmCharts = {
        upload:    _mkDualChart("chart-upload", "Race", "#f0883e", "Hoard", "#3fb950", v => formatBytes(v) + "/s"),
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
                    { label: "Race UL", data: [], borderColor: "#f0883e", backgroundColor: "#f0883e18", borderWidth: 1.5, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false },
                    { label: "Hoard UL", data: [], borderColor: "#3fb950", backgroundColor: "#3fb95018", borderWidth: 1.5, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false },
                    { label: "Race Peers", data: [], borderColor: "#bc8cff", backgroundColor: "#bc8cff18", borderWidth: 1, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false, borderDash: [4, 2] },
                    { label: "Hoard Peers", data: [], borderColor: "#58a6ff", backgroundColor: "#58a6ff18", borderWidth: 1, pointRadius: 0, pointHitRadius: 10, pointHoverRadius: 4, tension: 0.3, fill: false, borderDash: [4, 2] },
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
                            label: ctx => `${ctx.dataset.label}: ${ctx.parsed.y?.toFixed(1) ?? "—"} Mbps`,
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
                ? `${history.length} points · ${new Date(history[0].ts * 1000).toLocaleString()} → ${new Date(history[history.length-1].ts * 1000).toLocaleString()}`
                : "No data for this range";

        // Charts
        _updateDualChart(_bmCharts.upload, history, "race_upload_rate", "hoard_upload_rate");

        // Total uploaded — 20 stacked bars, each = sum of volume in bucket
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
        // Race Events — 20 stacked bars (added, completed, first_upload)
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

        // VPN speedtest — 7-day window, axis in days
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
    btn.textContent = "⏳ Running…";
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
        btn.textContent = "▶ Run now";
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
        errEl.textContent = "Fill in all three dates.";
        errEl.style.display = "block";
        return;
    }

    const start = new Date(startVal).getTime() / 1000;
    const mid   = new Date(midVal).getTime() / 1000;
    const end   = new Date(endVal).getTime() / 1000;

    if (start >= mid || mid >= end) {
        errEl.textContent = "Dates must be in order: start < middle < end.";
        errEl.style.display = "block";
        return;
    }

    try {
        const data = await api(`/api/benchmark/compare?start=${start}&mid=${mid}&end=${end}`);

        if (!data.metrics) {
            errEl.textContent = "No data for this range.";
            errEl.style.display = "block";
            return;
        }

        document.getElementById("cmp-counts").textContent =
            `P1: ${data.p1_count} samples — P2: ${data.p2_count} samples`;

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
                <td class="cmp-col-label">${meta.label}</td>
                <td>${meta.fmt(v.p1.avg)}</td>
                <td class="cmp-secondary">${meta.fmt(v.p1.max)}</td>
                <td>${meta.fmt(v.p2.avg)}</td>
                <td class="cmp-secondary">${meta.fmt(v.p2.max)}</td>
                <td style="color:${deltaColor};font-weight:700">${sign}${delta.toFixed(1)}%${arrow}</td>
            </tr>`;
        }).join("");

        resEl.style.display = "block";
    } catch (e) {
        errEl.textContent = "Error: " + e.message;
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
    b.title = any ? "" : "Enable a policy first";
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
        w.textContent = "Both thresholds are 0, so every race torrent past the keep floor matches — and the action is Delete. Their data will be erased.";
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
    } catch (e) { alert("Save failed: " + e.message); }
    // Thresholds decide whether the age/ratio policy can fire at all, so the
    // Drain now button has to be re-evaluated after any field edit too.
    _rpUpdateDrainBtn();
}
async function _rpRestart() {
    if (!confirm("Restart Hydra now to apply the race drain settings?")) return;
    try { await api("/api/settings/restart", { method: "POST" }); } catch (e) {}
}
async function _rpDrainNow(btn) {
    if (_rpUnboundedDelete() &&
        !confirm("Both thresholds are 0, so this deletes EVERY race torrent older than the keep floor, data included.\n\nRun it?")) return;
    btn.disabled = true; const t = btn.textContent; btn.textContent = "Draining…";
    let msg;
    try {
        const r = await api("/api/drain/now", { method: "POST" });
        msg = _rpDrainMsg(r);
    } catch (e) {
        msg = "Failed: " + (e && e.message ? e.message : "unknown error");
    }
    btn.disabled = false; btn.textContent = t;
    _rpUpdateDrainBtn();
    _rpDrainResult(msg);
    loadRacePolicy();
}
// Turn a /api/drain/now response into one line. A drain that legitimately did
// nothing must still say so — reporting nothing is what made the button look
// dead even when it had run.
function _rpDrainMsg(r) {
    if (!r || typeof r !== "object") return "Nothing to do.";
    if (r.status === "no_match") {
        if (r.no_category_link)
            return `${r.no_category_link} torrent(s) matched but their category has no hoard category linked — nothing moved.`;
        if (r.failed) return `${r.failed} torrent(s) matched but failed — see the logs.`;
        return "Nothing matched the thresholds.";
    }
    if (r.status === "no_drain_needed") return "Nothing to do: disk is below the start mark.";
    const n = r.removed_count || 0;
    if (!n) return "Nothing to do.";
    const verb = r.action === "hoard" ? "graduated" : "removed";
    let s = `${n} torrent(s) ${verb}, ${formatBytes(r.freed || 0)}.`;
    if (r.no_category_link) s += ` ${r.no_category_link} skipped (no linked category).`;
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
// SSE (status_snapshot, hoard_stats_snapshot) — this loop only handles the
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
    poll();
    updateCategories();
    fetchPublicIp();
    fetchPortForward();
    setInterval(poll, POLL_INTERVAL);
    setInterval(fetchPublicIp, 2 * 60 * 1000);
    setInterval(fetchPortForward, 60 * 1000);
    setupHoardSSE();
}

async function _checkStartup() {
    const overlay = document.getElementById("startup-overlay");
    try {
        const r = await fetch("/api/startup", { headers: { "X-Api-Key": API_KEY } });
        if (!r.ok) throw new Error();
        const d = await r.json();

        if (d.total > 0) {
            document.getElementById("startup-phase").textContent = "Restoring state…";
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
            // Accumulate into a persistent map (O(1)/row) and materialize the
            // array + counts + render ONCE at done. Rebuilding map+array per
            // batch was O(N^2) => ~24s at 106k though the server streams in <1s.
            if (!_hydMap) _hydMap = new Map(_hoardAllTorrents.map(t => [t.info_hash, t]));
            if (Array.isArray(data.torrents) && data.torrents.length) {
                for (const t of data.torrents) {
                    // Ratio against data-held (matches the detail panel). The
                    // server sends upload/download, which is 0 for our own
                    // uploads (download==0); recompute at ingest so the list
                    // and the sort agree with the detail.
                    if (t.total_size > 0 && t.total_done > 0) t.ratio = t.total_upload / t.total_done;
                    _hydMap.set(t.info_hash, t);
                    if (_resyncing) _resyncSeen.add(t.info_hash);
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
                try { _renderHoardCounts(); } catch (_) {}
                _scheduleHoardRender();
            }
            return;
        }
        if (type === "torrent_added" && data.info_hash) {
            if (_hoardAllTorrents && !_hoardAllTorrents.some(t => t.info_hash === data.info_hash)) {
                _hoardAllTorrents.unshift(data); // newest first; dynamic fields fill via stats_snapshot
                try { _renderHoardCounts(); } catch (_) {}
                _scheduleHoardRender();
            }
            return;
        }
        if (type === "stats_snapshot" && Array.isArray(data.torrents)) {
            if (!_hoardAllTorrents || !_hoardAllTorrents.length) return;
            const byHash = new Map();
            for (let i = 0; i < _hoardAllTorrents.length; i++) {
                byHash.set(_hoardAllTorrents[i].info_hash, _hoardAllTorrents[i]);
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
                _hoardAllTorrents = _hoardAllTorrents.filter(t => t.info_hash !== data.info_hash);
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

// Suspend SSE while tab is hidden — EventSource is not throttled by browser
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
    // TEST-NET-3 (203.0.113.0/24) — reserved for docs/examples, clearly fake.
    return "203.0.113." + (_incoHash(ip) % 254 + 1);
}
function incoPath(p) {
    // Save paths can leak real folders/usernames — replace with a plausible
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
        b.title = (_incognito ? "Incognito ON \u2014 " : "") + "Anonymize names, categories & IPs for screenshots";
    }
});

function esc(s) {
    return String(s).replace(/[&<>"']/g, ch => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[ch]));
}
// Regroupement des sections toml par domaine (ordre = ordre d'affichage).
// `tops` = sections toml de 1er niveau ; les sous-tables (race.custom_choking...) heritent du top.
const SETTINGS_DOMAINS = [
    { id: "daemon",   label: "General",              icon: "\u2699\uFE0F", tops: ["daemon"] },
    { id: "race",     label: "Session Race", icon: "\u{1F3C1}",     tops: ["race"] },
    { id: "hoard",    label: "Session Hoard",          icon: "\u{1F4E6}",     tops: ["hoard"] },
    { id: "trackers", label: "Trackers & Network",             icon: "\u{1F310}",     tops: ["announce_passkeys"] },
    { id: "observ",   label: "Observability",                 icon: "\u{1F4CA}",     tops: ["metrics", "peer_intel"] },
    { id: "maint",    label: "Maintenance",                   icon: "\u{1F9F9}",     tops: ["vpn_speedtest", "race_drain"] },
];
const _SETTINGS_DESC = {
    // [daemon]
    api_host: "IP the HTTP API/WebUI binds to (0.0.0.0 = all interfaces).",
    api_port: "TCP port for the HTTP API and WebUI.",
    api_key: "Secret required in the X-API-Key header to call the API.",
    data_dir: "Root directory for daemon state (categories, DBs, configs).",
    create_torrent_folder: "Create a per-torrent subfolder for single-file torrents (like qBittorrent's subfolder option). Off = single files saved directly in the category folder; multi-file torrents always keep their own folder. Applies to newly added torrents only.",
    // session (race/hoard)
    listen_port: "TCP/UDP port this session listens on for incoming peers.",
    listen_interfaces: "Comma-separated ip:port bind list (multi-homing).",
    listen_port_proxy_v2: "Extra listener expecting HAProxy PROXY-protocol v2 (real peer IP). 0 = off.",
    listen_addr_proxy_v2: "Explicit bind address for the PROXY-v2 listener. Empty = [::] wildcard.",
    proxy_v2_trusted_sources: "Source IPs allowed to send PROXY-v2 headers.",
    socks5_outbound_host: "SOCKS5 proxy host for this session's outbound peer connections.",
    socks5_outbound_port: "SOCKS5 proxy port for outbound peer connections.",
    socks5_outbound_user: "SOCKS5 username (if authenticated).",
    socks5_outbound_pass: "SOCKS5 password (if authenticated).",
    max_connections: "Global cap on simultaneous peer connections for this session.",
    max_uploads_per_torrent: "Max simultaneous upload slots per torrent (-1 = unlimited).",
    peer_timeout: "Seconds of inactivity before disconnecting a peer.",
    inactivity_timeout: "Seconds before an idle peer is considered inactive.",
    active_seeds: "Max torrents actively seeding (-1 = unlimited).",
    active_limit: "Max active torrents overall (-1 = unlimited).",
    active_downloads: "Max torrents actively downloading at once.",
    file_pool_size: "Max file handles kept open by the disk cache.",
    upload_rate_limit: "Upload speed cap in bytes/s (0 = unlimited).",
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
    "race::listen_port", "race::bind_interface", "race::max_connections", "race::max_uploads_per_torrent",
    "race::upload_rate_limit", "race::active_downloads", "race::active_seeds", "race::active_limit",
    "hoard::listen_port", "hoard::bind_interface", "hoard::max_connections", "hoard::max_uploads_per_torrent",
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
    "daemon::data_dir": "/configs",
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
        res.textContent = "Password too short (min 6).";
        return;
    }
    try {
        await api("/api/auth/password", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ password: pw }),
        });
        res.className = "result-msg success";
        res.textContent = "Password changed.";
        el.value = "";
    } catch (e) {
        res.className = "result-msg error";
        res.textContent = "Error: " + e.message;
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
        let html = _unitsCardHTML() + _interfacesCardHTML(_detectedIfaces) + `<div class="settings-toolbar">
            <input type="text" id="settings-search" class="settings-search" placeholder="Search a setting\u2026" oninput="filterSettings()">
            <label class="settings-adv-toggle"><span class="toggle"><input type="checkbox" id="settings-show-adv" onchange="filterSettings()"><span class="toggle-track"></span></span> Show advanced settings</label>
            <span id="settings-search-count" class="settings-search-count"></span>
        </div>`;

        const _activeDomSaved = localStorage.getItem("hydra_settings_tab");
        let _tabsHtml = "";
        let _panelsHtml = "";
        let _firstDom = null;
        for (const dom of order) {
            const sections = buckets[dom.id];
            if (!sections || !sections.length) continue;
            if (!_firstDom) _firstDom = dom.id;
            let count = 0;
            let body = "";
            for (const { path, scalars } of sections) {
                let rows = "";
                for (const [k, v] of scalars) {
                    const id = "set__" + path + "__" + k;
                    if (!Array.isArray(v)) _settingsOrig[id] = { section: path, key: k, value: v };
                    const search = (path + " " + k).toLowerCase();
                    const _adv = _SETTINGS_COMMON.has(path + "::" + k) ? "" : ' data-adv="1"';
                    const _def = _SETTINGS_DEFAULT[path + "::" + k];
                    const _dtxt = _SETTINGS_DESC[k] || "";
                    const _descHtml = (_dtxt || _def !== undefined)
                        ? `<span class="sr-desc">${esc(_dtxt)}${_def !== undefined ? ` <span class="sr-default">default: ${esc(String(_def))}</span>` : ""}</span>`
                        : "";
                    rows += `<div class="settings-row" data-search="${esc(search)}"${_adv}>
                        <div class="sr-label"><span class="sr-key">${esc(k)}</span>${_descHtml}<span class="sr-path">${esc(path)}</span></div>
                        <div class="sr-field">${_settingField(id, v, k)}</div>
                    </div>`;
                    count++;
                }
                body += `<div class="settings-section"><div class="settings-section-title">[${esc(path)}]</div>${rows}</div>`;
            }
            _tabsHtml += `<button type="button" class="settings-tab" data-domain="${dom.id}" onclick="showSettingsPanel('${dom.id}')"><span class="sg-title">${esc(dom.label)}</span> <span class="sg-count">${count}</span></button>`;
            _panelsHtml += `<div class="settings-panel" data-domain="${dom.id}"><div class="settings-group-body">${body}</div></div>`;
        }
        _tabsHtml += `<button type="button" class="settings-tab" data-domain="__import" onclick="showSettingsPanel('__import')"><span class="sg-title">Import</span></button>`;
        _panelsHtml += `<div class="settings-panel" data-domain="__import"><div class="settings-import"><h3 style="margin:0 0 .5em">Import from qBittorrent</h3><p class="sr-desc" style="margin:.4em 0 1em">Seeds data already on disk (completed torrents skip the hash-check) \u2014 nothing is re-downloaded. Already-present torrents are skipped, so it is safe to re-run.</p><button class="btn-primary" onclick="importFromQbit()">Import from qBittorrent</button></div></div>`;
        html += `<div class="settings-tabs" id="settings-tabs">${_tabsHtml}</div><div class="settings-panels" id="settings-panels">${_panelsHtml}</div>`;
        editor.innerHTML = html;
        {
            const _active = (_activeDomSaved && buckets[_activeDomSaved] && buckets[_activeDomSaved].length) ? _activeDomSaved : _firstDom;
            if (_active) showSettingsPanel(_active);
        }
        filterSettings();
        const banner = document.getElementById("settings-restart-banner");
        if (banner) banner.style.display = "none";
    } catch (e) {
        editor.textContent = "Error: " + e.message;
    }
}

// Filtre live : masque les lignes/sections/groupes sans match, ouvre les groupes qui matchent.
function showSettingsPanel(id) {
    document.querySelectorAll("#settings-panels .settings-panel").forEach((pn) => { pn.hidden = pn.dataset.domain !== id; });
    document.querySelectorAll("#settings-tabs .settings-tab").forEach((t) => { t.classList.toggle("active", t.dataset.domain === id); });
    localStorage.setItem("hydra_settings_tab", id);
}

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
    if (!changes.length) {
        banner.className = "result-msg info";
        banner.textContent = "No changes.";
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
                    hotApplied.push(`${ch.section} listen port`);
                } catch (e) { if (tier === "hot") tier = "engine"; } // fall back to restart
            }
        }
        const base = `${r.changed} setting(s) written to default.toml.`;
        banner.className = "result-msg success";
        if (tier === "hot") {
            banner.innerHTML = `${base} Applied live — no restart needed` +
                (hotApplied.length ? ` (${hotApplied.join(", ")})` : "") + ".";
        } else {
            const what = (tier === "full")
                ? "Daemon/auth settings changed — a full restart is required."
                : "Engine settings changed — restart the torrent engines to apply.";
            banner.innerHTML = `${base} ${what} ` +
                `<button class="btn-small btn-danger" onclick="restartDaemon()" style="margin-left:8px">Apply &amp; restart</button>`;
            if (_kc) banner.innerHTML += ' <span style="color:var(--text-secondary)">(API key updated for this browser)</span>';
        }
    } catch (e) {
        banner.className = "result-msg error";
        banner.textContent = "Error: " + e.message;
    }
}

async function restartDaemon() {
    if (!confirm("Restart the Hydra daemon now? (running torrents resume on boot)")) return;
    const banner = document.getElementById("settings-restart-banner");
    try {
        await api("/api/settings/restart", { method: "POST" });
        banner.className = "result-msg info";
        banner.textContent = "Restarting… the page will reload in a few seconds.";
        setTimeout(() => location.reload(), 7000);
    } catch (e) {
        banner.className = "result-msg error";
        banner.textContent = "Error: " + e.message;
    }
}

if (!API_KEY) promptLogin("Sign in to Hydra.");
if (API_KEY) maybeOfferImport();


// ─── Agents ─────────────────────────────────────────
let _editingAgent = null;
async function updateAgents() {
    updateRemovedAgents();
    try {
        const agents = await api("/api/agents");
        const tbody = document.getElementById("agents-tbody");
        if (!agents || !agents.length) {
            tbody.innerHTML = '<tr><td colspan="6" class="empty">No agents</td></tr>';
            return;
        }
        tbody.innerHTML = agents.map(a => {
            const dot = a.online
                ? '<span class="mode-tag mode-hoard">online</span>'
                : '<span class="mode-tag mode-race">offline</span>';
            const actions = a.kind === "local"
                ? '<span class="sr-desc">built-in</span>'
                : `<button class="btn-small" onclick="editAgent('${esc(a.name)}','${esc(a.addr || "")}')">Edit</button> <button class="btn-small btn-danger" onclick="deleteAgent('${esc(a.name)}')">Delete</button>`;
            const ifTip = (a.interfaces||[]).map(i=>i.name+": "+incoIP(i.ip)+(i.up?"":" (down)")).join("\n");
            return `<tr><td><strong>${esc(a.name)}</strong></td><td class="mono" style="font-size:12px">${esc(a.addr || "\u2014")}</td><td>${esc(a.kind)}${(a.engines||[]).length ? ' <span class="sr-desc">('+(a.engines||[]).map(e=>esc(e.id)+(e.online?'':' \u26a0')).join(', ')+')</span>' : ''}</td><td class="mono" style="font-size:12px" title="${esc(ifTip)}">${esc(incoExitIP(a.exit_ip) || "\u2014")}</td><td>${dot}</td><td>${actions}</td></tr>`;
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
    document.getElementById("agent-form").style.display = "block";
}
function hideAgentForm() { document.getElementById("agent-form").style.display = "none"; _editingAgent = null; }
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
    if (!p.addr) { _agResult("Address required", false); return; }
    try {
        const res = await api("/api/agents/test", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(p) });
        if (res.online) _agResult(" reachable", true);
        else _agResult("\u2717 unreachable: " + (res.error || ""), false);
    } catch (e) { _agResult("Error: " + e.message, false); }
}
async function saveAgent() {
    const p = _agentPayload();
    if (!p.name || !p.addr) { _agResult("Name and address required", false); return; }
    try {
        if (_editingAgent) {
            await api(`/api/agents/${encodeURIComponent(_editingAgent)}`, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(p) });
        } else {
            await api("/api/agents", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(p) });
        }
        hideAgentForm();
        await updateAgents();
    } catch (e) { _agResult("Error: " + e.message, false); }
}
async function deleteAgent(name) {
    if (!confirm(`Delete agent "${name}"?`)) return;
    try { await api(`/api/agents/${encodeURIComponent(name)}`, { method: "DELETE" }); await updateAgents(); }
    catch (e) { alert("Error: " + e.message); }
}

// Soft-delete safety: an accidentally removed agent stays here for one-click
// restore (its config is parked server-side; the remote agent never stops).
async function updateRemovedAgents() {
    const box = document.getElementById("agents-removed");
    if (!box) return;
    try {
        const rm = await api("/api/agents/removed");
        if (!rm || !rm.length) { box.innerHTML = ""; return; }
        box.innerHTML = '<div class="sr-desc" style="margin-top:12px">Recently removed \u2014 restore in one click</div>' +
            rm.map(a => `<div class="cat-agent-row"><span class="cat-agent-lbl"><strong>${esc(a.name)}</strong></span><span class="mono" style="font-size:12px">${esc(a.addr || "")}</span> <button class="btn-small" onclick="restoreAgent('${esc(a.name)}')">Restore</button></div>`).join("");
    } catch (e) { box.innerHTML = ""; }
}
async function restoreAgent(name) {
    try { await api(`/api/agents/restore/${encodeURIComponent(name)}`, { method: "POST" }); await updateAgents(); }
    catch (e) { alert("Error: " + e.message); }
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
async function updateTrackers() {
    try {
        const rows = await api("/api/trackers");
        const tbody = document.getElementById("trackers-tbody");
        if (!rows || !rows.length) {
            tbody.innerHTML = '<tr><td colspan="7" class="empty">No announces yet</td></tr>';
            return;
        }
        const _thtml = rows.map(r => {
            const status = r.ok
                ? '<span class="mode-tag mode-hoard">ok</span>'
                : '<span class="mode-tag mode-race">error</span>';
            const spoof = r.spoofed
                ? `<span class="mode-tag mode-hoard">${esc(r.peer_id_prefix || "spoof")}</span>`
                : '<span class="sr-desc">—</span>';
            const passkey = r.passkey_set
                ? '<span class="mode-tag mode-hoard">set</span>'
                : '<span class="sr-desc">—</span>';
            const err = r.last_error ? esc(r.last_error) : "—";
            return `<tr><td><strong>${esc(r.host)}</strong></td><td>${r.torrents}</td><td>${status}</td><td>${spoof}</td><td>${passkey}</td><td class="sr-desc" style="max-width:280px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis" title="${esc(r.last_error || "")}">${err}</td><td><button class="btn-small" onclick="editTracker('${esc(r.host)}','${esc(r.peer_id_prefix || "")}','${esc(r.user_agent || "")}')">Edit</button></td></tr>`;
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
        _trkStatsChart = _mkDualChart("trkstats-chart", "hoard ↑", "#2ea043", "race ↑", "#d29922", formatSpeed);
        _trkStatsChart.options.plugins.legend.display = true;
    }
    _trkStatsChart.data.labels = labels;
    _trkStatsChart.data.datasets[0].data = hoardSeries;
    _trkStatsChart.data.datasets[1].data = raceSeries;
    _trkStatsChart.update();
}

function showTrackerForm(host = "", pid = "", ua = "") {
    document.getElementById("trk-host").value = host;
    document.getElementById("trk-preset").value = "";
    document.getElementById("trk-pid").value = pid;
    document.getElementById("trk-ua").value = ua;
    document.getElementById("trk-passkey").value = "";
    document.getElementById("trk-result").style.display = "none";
    document.getElementById("trk-form").style.display = "block";
}
function hideTrackerForm() { document.getElementById("trk-form").style.display = "none"; }
function editTracker(host, pid, ua) { showTrackerForm(host, pid, ua); }
function _trkResult(msg, ok) {
    const r = document.getElementById("trk-result");
    r.textContent = msg; r.className = "result-msg " + (ok ? "success" : "error"); r.style.display = "block";
}
function _trkHost() { return document.getElementById("trk-host").value.trim(); }
async function saveTracker() {
    const host = _trkHost();
    if (!host) { _trkResult("Host required", false); return; }
    try {
        await api("/api/announce/clients", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, peer_id_prefix: document.getElementById("trk-pid").value.trim(), user_agent: document.getElementById("trk-ua").value.trim() }) });
        const pk = document.getElementById("trk-passkey").value.trim();
        if (pk) await api("/api/announce/passkeys", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, passkey: pk }) });
        hideTrackerForm(); await updateTrackers();
    } catch (e) { _trkResult("Error: " + e.message, false); }
}
async function clearTrackerSpoof() {
    const host = _trkHost();
    if (!host) { _trkResult("Host required", false); return; }
    try { await api("/api/announce/clients", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, peer_id_prefix: "" }) }); hideTrackerForm(); await updateTrackers(); }
    catch (e) { _trkResult("Error: " + e.message, false); }
}
async function clearTrackerPasskey() {
    const host = _trkHost();
    if (!host) { _trkResult("Host required", false); return; }
    try { await api("/api/announce/passkeys", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ host, passkey: "" }) }); hideTrackerForm(); await updateTrackers(); }
    catch (e) { _trkResult("Error: " + e.message, false); }
}

// --- Local engines (shards) ---

function showEngineForm(){ document.getElementById("engine-form").style.display="block"; }
function hideEngineForm(){ document.getElementById("engine-form").style.display="none"; }
async function updateEngines(){
    try{
        const results = await Promise.all([api("/api/agents"), api("/api/engines")]);
        const agents = results[0] || [];
        const extras = results[1] || [];
        const tb = document.getElementById("engines-tbody");
        if(!tb) return;
        const rows = [];
        // Base engines (the built-in local race/hoard) — shown but not deletable.
        const local = agents.find(function(a){ return a.name === "local"; });
        if(local && local.engines){
            local.engines.forEach(function(e){
                rows.push("<tr><td><strong>" + esc(e.id) + "</strong></td><td>" + esc(e.role) +
                          "</td><td>base</td><td><span class=\"sr-desc\">built-in</span></td></tr>");
            });
        }
        // Extra engines (shards) — deletable.
        extras.forEach(function(e){
            rows.push("<tr><td><strong>" + esc(e.id) + "</strong></td><td>" + esc(e.role) + "</td><td>" + e.listen_port +
                      "</td><td><button class=\"btn-small btn-danger\" onclick=\"deleteEngine('" + esc(e.id) + "')\">Delete</button></td></tr>");
        });
        tb.innerHTML = rows.length ? rows.join("") : '<tr><td colspan="4" class="empty">No engines</td></tr>';
    }catch(err){ console.error("updateEngines", err); }
}
async function addEngine(){
    const id = document.getElementById("eng-id").value.trim();
    const role = document.getElementById("eng-role").value;
    const port = parseInt(document.getElementById("eng-port").value) || 0;
    if(!id){ alert("id required"); return; }
    try{
        await api("/api/engines", {method:"POST", headers:{"Content-Type":"application/json"}, body:JSON.stringify({id:id, role:role, listen_port:port})});
        hideEngineForm();
        document.getElementById("eng-id").value="";
        document.getElementById("restart-banner").style.display="block";
        updateEngines();
    }catch(err){ alert("add engine failed: " + err); }
}
async function deleteEngine(id){
    if(!confirm("Delete engine " + id + "? Its torrents stop seeding after restart.")) return;
    try{
        await api("/api/engines/" + encodeURIComponent(id), {method:"DELETE"});
        document.getElementById("restart-banner").style.display="block";
        updateEngines();
    }catch(err){ alert("delete failed: " + err); }
}
async function restartHydra(){
    if(!confirm("Restart Hydra to apply engine changes? (~40s)")) return;
    try{ await api("/api/restart", {method:"POST"}); }catch(err){}
    alert("Restarting \u2014 reconnect in ~40s.");
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
    if (info) info.textContent = `(${have}/${total} — ${missing} missing)`;

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
        { id: "total_size", label: "Size", sort: "total_size", render: t => `<td>${t.total_size ? formatBytes(t.total_size) : "—"}</td>` },
        { id: "state", label: "State", sort: "state", render: t => { const d = displayState(t); return `<td><span class="state-badge ${d.cls}" title="${d.title}">${d.label}</span></td>`; } },
        { id: "progress", label: "Progress", sort: "progress", render: t => { const pct = (t.progress * 100).toFixed(1); return `<td><div class="progress-bar"><div class="progress-fill" style="width:${pct}%"></div></div><div class="progress-text">${pct}%</div></td>`; } },
        { id: "swarm_seeds", label: "Seeds", sort: "swarm_seeds", render: t => `<td>${t.swarm_seeds ?? "—"}</td>` },
        { id: "swarm_leechers", label: "Leechers", sort: "swarm_leechers", render: t => `<td>${t.swarm_leechers ?? "—"}</td>` },
        { id: "download_rate", label: "Down", sort: "download_rate", render: t => `<td>${formatSpeed(t.download_rate ?? 0)}</td>` },
        { id: "upload_rate", label: "Up", sort: "upload_rate", render: t => `<td>${formatSpeed(t.upload_rate)}</td>` },
        { id: "ratio", label: "Ratio", sort: "ratio", render: t => `<td>${displayRatio(t).toFixed(2)}</td>` },
        { id: "tracker_host", label: "Tracker", sort: "tracker_host", render: t => `<td>${esc(t.tracker_host || "—")}</td>` },
        { id: "category", label: "Category", sort: "category", render: t => `<td>${esc(incoCat(t.category))}</td>` },
        { id: "tags", label: "Tags", sort: null, render: t => `<td>${(t.tags && t.tags.length) ? esc(t.tags.join(", ")) : "—"}</td>` },
        { id: "added_time", label: "Added", sort: "added_time", render: t => `<td>${formatDate(t.added_time)}</td>` },
        { id: "completed_time", label: "Completed", sort: "completed_time", render: t => `<td>${formatDate(t.completed_time)}</td>` },
        { id: "agent", label: "Agent", sort: "agent", render: t => `<td>${esc(t.agent || "local")}</td>` },
    ],
    "race-table": [
        { id: "name", label: "Name", sort: "name", render: t => `<td title="${t.info_hash}">${esc(incoName(t))}${t.tracker_error ? ' <span class="tracker-warn" title="Tracker error">!</span>' : ''}${t.injected_peers ? ` <span class="uploader-badge ${t.injection_hit ? 'injection-hit' : ''}" title="Uploader: ${t.uploader} - ${t.injected_peers} peers injected${t.injection_hit ? ' HIT' : ''}">${t.injection_hit ? '&#9889;&#10003;' : '&#9889;'}${t.injected_peers}</span>` : ''}</td>` },
        { id: "total_size", label: "Size", sort: "total_size", render: t => `<td>${t.total_size ? formatBytes(t.total_size) : "—"}</td>` },
        { id: "progress", label: "Progress", sort: "progress", render: t => { const pct = (t.progress * 100).toFixed(1); return `<td><div class="progress-bar"><div class="progress-fill" style="width:${pct}%"></div></div><div class="progress-text">${pct}%</div></td>`; } },
        { id: "swarm_seeds", label: "Seeds", sort: "swarm_seeds", render: t => `<td>${t.swarm_seeds ?? "—"}</td>` },
        { id: "swarm_leechers", label: "Leechers", sort: "swarm_leechers", render: t => `<td>${t.swarm_leechers ?? "—"}</td>` },
        { id: "download_rate", label: "Down", sort: "download_rate", render: t => `<td>${formatSpeed(t.download_rate)}</td>` },
        { id: "upload_rate", label: "Up", sort: "upload_rate", render: t => `<td>${formatSpeed(t.upload_rate)}</td>` },
        { id: "ratio", label: "Ratio", sort: "ratio", render: t => `<td>${displayRatio(t).toFixed(2)}</td>` },
        { id: "tracker_host", label: "Tracker", sort: "tracker_host", render: t => `<td>${esc(t.tracker_host || "—")}</td>` },
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
        return `<th class="col-drag${cls}" draggable="true" data-colid="${c.id}"${sortable}>${esc(c.label)}</th>`;
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
    menu.innerHTML = `<div class="ctx-label">Columns</div><div class="ctx-separator"></div><div class="ctx-scroll">` +
        cfg.order.map(id => {
            const col = byId[id]; if (!col) return "";
            const on = !hidden.has(id);
            return `<div class="ctx-item" onclick="toggleColumn('${tableId}','${id}',this)"><span class="col-chk">${on ? "✓" : ""}</span>${esc(col.label)}</div>`;
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
        .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
    const out = [];
    let inList = false;
    const closeList = () => { if (inList) { out.push("</ul>"); inList = false; } };
    for (const raw of md.split("\n")) {
        const line = raw.replace(/\s+$/, "");
        if (/^### /.test(line)) { closeList(); out.push("<h3>" + inline(line.slice(4)) + "</h3>"); }
        else if (/^## /.test(line)) { closeList(); out.push("<h2>" + inline(line.slice(3)) + "</h2>"); }
        else if (/^# /.test(line)) { closeList(); out.push("<h1>" + inline(line.slice(2)) + "</h1>"); }
        else if (/^---+$/.test(line)) { closeList(); out.push("<hr>"); }
        else if (/^[-*] /.test(line)) { if (!inList) { out.push("<ul>"); inList = true; } out.push("<li>" + inline(line.slice(2)) + "</li>"); }
        else if (line.trim() === "") { closeList(); }
        else { closeList(); out.push("<p>" + inline(line) + "</p>"); }
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
    } catch (e) { el.textContent = "Failed to load changelog: " + e.message; }
}

let _lastUpdateCheck = 0;
async function checkForUpdate() {
    const el = document.getElementById("update-badge");
    if (!el) return;
    // Server caches the GitHub lookup 6h, so polling it every health tick is
    // pointless — throttle the client to once per 30 min.
    if (_lastUpdateCheck && Date.now() - _lastUpdateCheck < 1800000) return;
    _lastUpdateCheck = Date.now();
    try {
        const d = await api("/api/update-check");
        if (d && d.enabled && d.update_available) {
            el.innerHTML = ` <a href="${d.url || '#'}" target="_blank" rel="noopener" class="update-badge" title="A newer version (${esc(d.latest)}) is available">update ${esc(d.latest)}</a>`;
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
    el.textContent = logsEntries.length ? logsEntries.map(fmtLogLine).join("\n") : "(no matching entries)";
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
        if (el) el.textContent = "Failed to load logs: " + e.message;
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
    const body = "**Describe the issue**\n\n\n**Version:** (see the startup banner)\n\n**Logs** (paste from the Logs tab — check for your public IP before posting):\n```\n\n```\n";
    a.href = "https://github.com/Kheopsian/Hydra/issues/new?title=" + encodeURIComponent("[bug] ") + "&body=" + encodeURIComponent(body);
}
