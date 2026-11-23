"use strict";
const fs = require("fs"), path = require("path"), assert = require("assert");
const src = fs.readFileSync(process.env.APP_JS || path.join(__dirname,"../static/app.js"),"utf8").replace(/\r\n/g,"\n");
function fn(name, optional = false) {
    const match = new RegExp("^(?:async )?function " + name + "\\(", "m").exec(src);
    if (!match && optional) return `function ${name}() {}`;
    assert.ok(match, name);
    return src.slice(match.index, src.indexOf("\n}",match.index)+2);
}
async function main() {
    const env = {visible:true, hidden:false, pending:[], calls:[], elements:new Map(), extras:[], errors:[], defer:true};
    const view = new Function("env", `
        let _lastStatusSnapshot = null, _sseConn = null, _syncCursor = 0, _lastStatusEventAt = 0;
        const _extraTorrents = 0, window = {}, _bmSpan = 60, _EXTRA_ENGINE_COLORS = ["#fff"];
        const document = {get hidden(){return env.hidden;},getElementById:id=>{
            if(id==="tab-benchmark")return {classList:{contains:()=>env.visible}};
            if(!env.elements.has(id))env.elements.set(id,{textContent:"old",style:{},classList:{}});
            return env.elements.get(id);
        }};
        const EventSource = class {}, console = {error:(...args)=>env.errors.push(args)};
        const formatSpeed=String, fmtInt=String, formatBytes=String, formatUptime=String, t=String;
        const _applyHoardLiveCounts=()=>{}, _renderExtraEngines=()=>{};
        const _renderExtraBenchCards = rows => env.extras.push(rows);
        const chart=()=>({data:{labels:[],datasets:Array.from({length:6},()=>({data:[]}))},update(){}});
        const _bmCharts={upload:chart(),uploading:chart(),totalUploaded:chart(),announce:chart(),raceEvents:chart(),iowait:chart(),arcMiss:chart(),vpn:chart()};
        const _updateDualChart=()=>{}, _updateChart=()=>{}, _renderAnnounceFigures=()=>{}, _movingAvg=()=>[];
        const api=url=>{
            env.calls.push(url);
            if(url.includes("/benchmark/range")||url.includes("/benchmark/engines")) {
                const value=url.includes("/engines")?{}:[];
                if(!env.defer)return Promise.resolve(value);
                return new Promise(resolve=>env.pending.push(()=>resolve(value)));
            }
            return Promise.resolve(url.includes("/history")?[]:null);
        };
        ${fn("_renderBenchmarkLive",true)}
        ${fn("_renderStatus")}
        ${fn("_openHoardSSE")}
        ${fn("updateBenchmark")}
        _openHoardSSE();
        return {status:data=>_sseConn.onmessage({data:JSON.stringify({event:"status_snapshot",data})}),history:updateBenchmark};
    `)(env);
    const frame = rate => ({race:{total_upload_rate:rate,total_download_rate:7,total_peers:4},
        hoard:{active_upload_rate:30,active_download_rate:0},
        engines:[{id:"vpn",stats:{upload_rate:rate+1,download_rate:2,peers:3}}],
        performance:{measured:true,iowait_pct:1.5,arc_hit_rate_pct:90,arc_miss_per_sec:2,arc_size_bytes:4096}});
    const history = view.history();
    view.status(frame(20));
    const value=id=>env.elements.get(id)?.textContent;
    assert.equal(value("total-upload"),"50","the header receives the current frame");
    assert.equal(value("bm-race-upload"),"20","Performance cards must follow SSE while graph history is stalled");
    assert.equal(value("bm-hoard-upload"),"30");
    assert.equal(value("bm-race-download"),"7");
    assert.equal(value("bm-race-peers"),"4");
    assert.equal(value("bm-iowait"),"1.5%");
    assert.equal(value("bm-arc-hit"),"90.000%");
    assert.equal(value("bm-arc-miss"),"2.0/s");
    assert.equal(value("bm-arc-size"),"4096");
    assert.equal(env.extras.at(-1)[0].upload_rate,21,"extra engines share the live path");
    view.status(frame(50));
    assert.equal(value("bm-race-upload"),"50","the next frame updates cards without another history fetch");
    env.pending.forEach(resolve=>resolve()); await history;
    assert.equal(value("bm-race-upload"),"50","late history responses cannot roll live cards back");
    assert.ok(!env.calls.includes("/api/benchmark/current"),"healthy SSE needs no duplicate current-metrics request");
    assert.equal(env.errors.length,0,"history processing still completes: " + env.errors.map(args=>args.map(String).join(": ")).join("; "));
    env.visible=false;view.status(frame(60));
    assert.equal(value("bm-race-upload"),"50","hidden Performance cards are not repainted");
    env.visible=true;env.defer=false;
    const reopened=view.history();
    assert.equal(value("bm-race-upload"),"60","opening Performance paints the latest cached status immediately");
    await reopened;
    const stopped = frame(0); stopped.performance.measured = false;
    view.status(stopped);
    assert.equal(value("bm-race-upload"),"0","zero rates are painted instead of preserving a stale speed");
    assert.equal(value("bm-iowait"),"-","unmeasured system gauges are not reported as measured zeros");
    console.log("performance_live: ok");
}
main().catch(e=>{console.error(e);process.exitCode=1;});
