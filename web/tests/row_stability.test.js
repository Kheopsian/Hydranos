"use strict";
const assert = require("assert");
const fs = require("fs");
const path = require("path");
const src = fs.readFileSync(process.env.APP_JS || path.join(__dirname, "../static/app.js"), "utf8").replace(/\r\n/g, "\n");
function fn(name, optional = false) {
    const match = new RegExp("^(?:async )?function " + name + "\\(", "m").exec(src);
    if (!match && optional) return `function ${name}(rows) {return rows;}`;
    assert.ok(match, name);
    return src.slice(match.index, src.indexOf("\n}", match.index) + 2);
}
class Element {
    constructor(tag) { this.tag = tag; this.children = []; this.dataset = {}; this.writes = 0; this.classList = {toggle() {}}; }
    set innerHTML(value) {
        this.html = value; this.writes++;
        if (this.tag === "tbody") {
            this.children = [...value.matchAll(/data-hash="([^"]+)"/g)].map(m => {
                const row = new Element("tr"); row.dataset.hash = m[1]; row.parent = this; return row;
            });
        } else if (this.tag === "tr") {
            this.children = [...value.matchAll(/<td>(.*?)<\/td>/g)].map(m => {
                const cell = new Element("td"); cell.html = m[1]; cell.parent = this; return cell;
            });
        }
    }
    get innerHTML() { return this.html; }
    get outerHTML() { return `<${this.tag}>${this.html || ""}</${this.tag}>`; }
    get lastElementChild() { return this.children.at(-1); }
    get firstElementChild() { return this.children[0]; }
    insertBefore(row, before) {
        row.remove();
        const index = before ? this.children.indexOf(before) : this.children.length;
        this.children.splice(index, 0, row); row.parent = this;
    }
    appendChild(cell) { this.insertBefore(cell, null); }
    replaceChild(cell, old) {
        const index = this.children.indexOf(old);
        cell.remove(); old.parent = null;
        this.children.splice(index, 1, cell); cell.parent = this;
    }
    remove() {
        if (!this.parent) return;
        this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null;
    }
}
const tbody = new Element("tbody");
const env = {rows: [{agent: "local-hoard", info_hash: "a", state: "seeding", upload_rate: 0},
    {agent: "local-hoard", info_hash: "b", state: "seeding", upload_rate: 20}]};
const view = new Function("env", "tbody", "Element", `
    let _hoardAllTorrents, _hoardFiltered, _hoardAppliedSearch = "", _hoardServerFiltered = 2, _hoardServerTotal = 1000, _hoardPageApplied = 1;
    const HOARD_RENDER_LIMIT = 500, _hoardSortCol = "upload_rate", _hoardSortAsc = false;
    const _hoardMatches = row => row.upload_rate > 0;
    const _hoardCmp = () => (a,b) => b.upload_rate-a.upload_rate, _topKSorted = (rows,k,cmp) => rows.slice().sort(cmp).slice(0,k);
    const document = {getElementById: id => id === "hoard-tbody" ? tbody : null, createElement: tag => new Element(tag)};
    const _rowCellHtml = (table, row) => ["<td>" + row.info_hash + "</td>", "<td>" + row.upload_rate + "</td>"];
    const renderRowCells = (table, row) => _rowCellHtml(table, row).join("");
    const esc = String, t = String, _visibleCols = () => [], _updateRowHighlights = () => {};
    const handleRowClick = () => {}, handleRowContextMenu = () => {};
    ${fn("_renderHoardRows", true)}
    ${fn("renderHoardTable")}
    return () => { _hoardAllTorrents = env.rows; renderHoardTable(); };
`)(env, tbody, Element);
view();
assert.deepEqual(tbody.children.map(r => r.dataset.hash), ["a", "b"], "SSE figures must not remove or reorder rows from the accepted server page");
const first = tbody.children[0], second = tbody.children[1], writes = second.writes;
const nameCell = second.children[0];
view();
assert.equal(tbody.children[0], first, "unchanged rows keep their DOM identity");
assert.equal(second.writes, writes, "unchanged row contents are not rebuilt");
env.rows[1].upload_rate = 30;
view();
assert.equal(tbody.children[1], second, "rate updates keep row identity");
assert.equal(second.children[1].innerHTML, "30", "changed rates are painted");
assert.equal(second.children[0], nameCell, "rate changes preserve the name cell and its interactive contents");
env.rows = [env.rows[1]];
view();
assert.deepEqual(tbody.children.map(r => r.dataset.hash), ["b"], "an accepted membership change removes the departed row");

const reconcile = new Function(`const HOARD_RENDER_LIMIT = 500, _hoardAppliedSearch = "", _hoardMatches = row => row.upload_rate > 0;
    ${fn("_reconcileHoardRows", true)} return _reconcileHoardRows;`)();
const old = {agent: "local-hoard", info_hash: "same", upload_rate: 20, _liveRevision: 2, _liveUpdatedAt: 1000};
const kept = reconcile([], [old], 1, 500, true);
assert.equal(kept.length, 1, "newer matching SSE data survives an older background page omission");
assert.equal(reconcile([], kept, 2, 2000, true).length, 0, "older SSE data cannot keep a genuinely departed row");
assert.equal(reconcile([], kept, 1, 500, false).length, 0, "manual refreshes and filter changes do not retain old rows");
assert.equal(reconcile([], [{...old, upload_rate: 0}], 1, 500, true).length, 0, "confirmed inactive rows leave the active view");
const anotherEngine = {...old, agent: "local-vpn"};
assert.equal(reconcile([anotherEngine], [old], 1, 500, true).length, 2, "different engines holding the same hash remain distinct");
const fullPage = Array.from({length: 500}, (_, i) => ({info_hash: String(i)}));
assert.equal(reconcile(fullPage, [old], 1, 500, true), fullPage, "a ranked page boundary cannot grow beyond its limit");
console.log("row_stability: ok");
