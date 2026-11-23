// The first-run import offer must not come back around an import (#102).
// Run with: node web/tests/import_offer.test.js (from the repository root).
// Pulls maybeOfferImport() and its decision out of app.js, so the test reads
// the shipped code, not a copy of it.
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const file = process.env.APP_JS || path.join(__dirname, "..", "static", "app.js");
const src = fs.readFileSync(file, "utf8");
const start = src.indexOf("function shouldOfferImport(");
assert.ok(start >= 0, "shouldOfferImport not found in app.js");
const fnStart = src.indexOf("function maybeOfferImport(", start);
assert.ok(fnStart >= 0, "maybeOfferImport not found in app.js");
const end = src.indexOf("\n}\n", fnStart);

// Runs the page-load offer against canned answers; says whether the wizard opened.
async function offered({ provenance, status, dismissed = null }) {
    let opened = 0;
    const fetch = (url) => {
        if (url === "/api/provenance") return Promise.resolve({ ok: true, json: () => Promise.resolve(provenance) });
        if (url === "/api/import/qbit/status") {
            if (status === "error") return Promise.reject(new Error("offline"));
            return Promise.resolve({ ok: true, json: () => Promise.resolve(status) });
        }
        throw new Error("unexpected " + url);
    };
    const localStorage = { getItem: () => dismissed };
    const body = src.slice(start, end + 2) + "\nreturn { maybeOfferImport };";
    const mod = new Function("fetch", "localStorage", "importWizard", "API_KEY",
        "let _provenance = null;\n" + body)(fetch, localStorage, () => { opened++; }, "k");
    mod.maybeOfferImport();
    for (let i = 0; i < 10; i++) await new Promise(r => setImmediate(r));
    return opened > 0;
}

(async () => {
    const none = { present: false };
    // A fresh instance: offered.
    assert.strictEqual(await offered({ provenance: none, status: { running: false } }), true, "fresh instance is offered the import");
    // A reload while an import runs: not offered.
    assert.strictEqual(await offered({ provenance: none, status: { running: true, job_id: "imp-1", phase: "torrents" } }), false,
        "not offered while an import is running");
    // An import that ended with nothing added (no provenance written): not offered.
    assert.strictEqual(await offered({ provenance: none, status: { running: false, job_id: "imp-1", phase: "done", failed: 3 } }), false,
        "not offered after an import that added nothing");
    // Provenance recorded, or the offer dismissed: not offered.
    assert.strictEqual(await offered({ provenance: { present: true, source_client: "qBittorrent" }, status: { running: false } }), false);
    assert.strictEqual(await offered({ provenance: none, status: { running: false }, dismissed: "1" }), false);
    // A status that cannot be read falls back to provenance alone.
    assert.strictEqual(await offered({ provenance: none, status: "error" }), true, "an unreadable status does not hide the offer");
    console.log("import_offer: ok");
})().catch(e => { console.error(e); process.exit(1); });
