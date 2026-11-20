// The category edit form fills at once from the table, and the server's answer
// never rewrites what the person is typing nor lands in another opening.
// Run with: node web/tests/category_form_fill.test.js (from the repository
// root). Pulls the form's functions out of app.js, so the test reads the
// shipped code, not a copy of it.
"use strict";
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const file = process.env.APP_JS || path.join(__dirname, "..", "static", "app.js");
const src = fs.readFileSync(file, "utf8");

// A top-level function of app.js, from its line to the first lone "}".
function grab(name, required) {
    const m = new RegExp("^(async )?function " + name + "\\(", "m").exec(src);
    if (!m) {
        assert.ok(!required, name + " not found in app.js");
        return "";
    }
    return src.slice(m.index, src.indexOf("\n}\n", m.index) + 2) + "\n";
}
const code = ["showCategoryForm", "hideCategoryForm", "_catModeChanged"].map(n => grab(n, true)).join("")
    + grab("_catFillForm", false);

function harness(seen) {
    const els = {};
    const el = (id) => els[id] || (els[id] = { id, value: "", checked: false, style: { display: "" }, innerHTML: "", textContent: "" });
    const document = { getElementById: el };
    const answers = []; // pending /api/categories calls, oldest first
    const api = (url) => {
        if (url === "/api/categories") return new Promise(r => answers.push(r));
        throw new Error("unexpected " + url);
    };
    const make = new Function("document", "api", "console", `
        let _editingCategory = null, _editingCategoryStored = null, _fsBrowsedPath = null;
        let _categoriesSeen = null, _catFormStamp = 0, _catFormTouched = false;
        let _catChoices = [], _catUnreachable = [], _catTargets = [], _catChoicesStamp = 0;
        const _CAT_SPREADS = ["bytes", "count", "fill", "free_space"];
        function _populateGraduateSelect(cats, current) { document.getElementById("cat-graduate-to").value = current || ""; }
        function _catBytesToGb(b) { return b ? String(b / 1073741824) : ""; }
        async function _catTargetChoices() { return { choices: [], unreachable: [] }; }
        function _catRenderTargets() {}
        function _catPruneTargets(t) { return t; }
        ${code}
        return {
            showCategoryForm, hideCategoryForm,
            seen(v) { _categoriesSeen = v; },
            stored() { return _editingCategoryStored; },
        };`);
    const ui = make(document, api, { error() {} });
    ui.seen(seen);
    const form = el("category-form");
    const touch = (id, value, ev) => {
        el(id).value = value;
        const h = form[ev || "oninput"];
        if (h) h({ target: el(id) });
    };
    const settle = () => new Promise(r => setImmediate(r));
    return { el, ui, answers, touch, settle };
}

const TV = { name: "tv", mode: "race", save_path: "/data/tv", graduate_to: "" };
const MOVIES = { name: "movies", mode: "hoard", save_path: "/data/movies", transit: true };

(async () => {
    // 1. Shown at once with what the table holds, before the server answers.
    {
        const h = harness([TV, MOVIES]);
        h.ui.showCategoryForm("movies");
        await h.settle();
        assert.strictEqual(h.answers.length, 1, "the server is still asked");
        assert.strictEqual(h.el("cat-mode").value, "hoard", "mode filled before the server answers");
        assert.strictEqual(h.el("cat-save-path").value, "/data/movies", "save path filled before the server answers");
        assert.strictEqual(h.el("cat-transit").checked, true);
    }

    // 2. What the person typed before the answer stays; the hidden fields the
    //    PUT sends back are the server's.
    {
        const h = harness([TV, MOVIES]);
        const done = h.ui.showCategoryForm("tv");
        await h.settle();
        h.touch("cat-save-path", "/typed/here");
        h.answers[0]([{ ...TV, save_path: "/server/tv", placement: "kept" }, MOVIES]);
        await done;
        assert.strictEqual(h.el("cat-save-path").value, "/typed/here", "a path typed before the answer is kept");
        assert.strictEqual(h.ui.stored().placement, "kept", "the stored entry is the server's");
    }

    // 3. A form nobody touched takes the server's newer values.
    {
        const h = harness([TV, MOVIES]);
        const done = h.ui.showCategoryForm("tv");
        await h.settle();
        h.answers[0]([{ ...TV, save_path: "/server/tv" }, MOVIES]);
        await done;
        assert.strictEqual(h.el("cat-save-path").value, "/server/tv");
    }

    // 4. A mode changed on the server, form untouched: the fields that follow
    //    the mode follow it too.
    {
        const h = harness([TV, MOVIES]);
        const done = h.ui.showCategoryForm("tv");
        await h.settle();
        assert.strictEqual(h.el("cat-graduate-wrap").style.display, "", "race shows graduate-to");
        h.answers[0]([{ ...TV, mode: "hoard", transit: true }, MOVIES]);
        await done;
        assert.strictEqual(h.el("cat-mode").value, "hoard");
        assert.strictEqual(h.el("cat-transit").checked, true);
        assert.strictEqual(h.el("cat-graduate-wrap").style.display, "none", "hoard hides graduate-to");
        assert.strictEqual(h.el("cat-transit-wrap").style.display, "", "hoard shows transit");
    }

    // 5. A mode changed by hand before the answer: neither the mode nor what
    //    depends on it is put back.
    {
        const h = harness([TV, MOVIES]);
        const done = h.ui.showCategoryForm("movies");
        await h.settle();
        h.touch("cat-mode", "race", "onchange");
        h.answers[0]([TV, { ...MOVIES, transit: false }]);
        await done;
        assert.strictEqual(h.el("cat-mode").value, "race", "the mode chosen by hand stays");
        assert.strictEqual(h.el("cat-transit").checked, true, "transit is not reset by the old mode's answer");
    }

    // 6. An answer for an earlier opening is dropped: A then B, and A's answer
    //    arrives last; A, closed, A again.
    {
        const h = harness([TV, MOVIES]);
        const a = h.ui.showCategoryForm("tv");
        await h.settle();
        const b = h.ui.showCategoryForm("movies");
        await h.settle();
        h.answers[1]([TV, MOVIES]);
        await b;
        h.answers[0]([{ ...TV, mode: "race", save_path: "/from/a", placement: "a" }, { ...MOVIES, save_path: "/from/a" }]);
        await a;
        assert.strictEqual(h.el("cat-save-path").value, "/data/movies", "A's answer does not fill B's form");
        assert.strictEqual(h.ui.stored().name, "movies");
        assert.strictEqual(h.ui.stored().placement, undefined, "nor its stored entry");

        const h2 = harness([TV, MOVIES]);
        const first = h2.ui.showCategoryForm("tv");
        await h2.settle();
        h2.ui.hideCategoryForm();
        const second = h2.ui.showCategoryForm("tv");
        await h2.settle();
        h2.answers[0]([{ ...TV, save_path: "/stale" }, MOVIES]);
        await first;
        assert.strictEqual(h2.el("cat-save-path").value, "/data/tv", "the first opening's answer does not fill the second");
        h2.answers[1]([TV, MOVIES]);
        await second;

        // Closed and not reopened: the answer leaves the closed form alone.
        const h3 = harness([TV, MOVIES]);
        const lone = h3.ui.showCategoryForm("tv");
        await h3.settle();
        h3.ui.hideCategoryForm();
        h3.answers[0]([{ ...TV, placement: "late" }, MOVIES]);
        await lone;
        assert.strictEqual(h3.ui.stored(), null, "a closed form keeps no stored entry from a late answer");
    }

    console.log("category_form_fill: ok");
})().catch(e => { console.error(e); process.exit(1); });
