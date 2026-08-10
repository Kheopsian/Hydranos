// Hydra i18n.
//
// The key IS the English text: t("Add torrent"), never t("torrent.add").
// Three things follow from that, and they are the whole reason for the choice:
// English can never regress (a missing entry renders the key, which is already
// correct English), there is no en.json to keep in sync, and wrapping a string
// costs nothing more than typing t() around it.
//
// Static markup in index.html is not annotated. The DOM is walked once at boot,
// before app.js injects anything, so everything present at that moment came from
// the template and is safe to translate. Dynamic strings go through t().

(function (global) {
  "use strict";

  var STORAGE_KEY = "hydra_lang";
  var FALLBACK = "en";

  // Languages we ship. English is implicit and has no file.
  var LANGUAGES = [
    { code: "en", label: "English" },
    { code: "fr", label: "Francais" },
    { code: "de", label: "Deutsch" },
    { code: "es", label: "Espanol" },
    { code: "it", label: "Italiano" },
    { code: "pt", label: "Portugues" },
    { code: "nl", label: "Nederlands" }
  ];

  var dict = {};
  var current = FALLBACK;

  function supported(code) {
    if (!code) return null;
    code = String(code).toLowerCase();
    for (var i = 0; i < LANGUAGES.length; i++) {
      if (LANGUAGES[i].code === code) return code;
    }
    var base = code.split("-")[0];          // fr-CA -> fr, zh-Hans -> zh
    for (var j = 0; j < LANGUAGES.length; j++) {
      if (LANGUAGES[j].code === base) return base;
    }
    return null;
  }

  // Stored choice wins over the browser: someone on a French laptop may still
  // want the English UI, and that preference has to survive a reload.
  function detect() {
    var stored = null;
    try { stored = localStorage.getItem(STORAGE_KEY); } catch (e) {}
    var fromStore = supported(stored);
    if (fromStore) return fromStore;
    var navs = (global.navigator && (navigator.languages || [navigator.language])) || [];
    for (var i = 0; i < navs.length; i++) {
      var hit = supported(navs[i]);
      if (hit) return hit;
    }
    return FALLBACK;
  }

  function interpolate(str, vars) {
    if (!vars) return str;
    return str.replace(/\{(\w+)\}/g, function (m, name) {
      return Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : m;
    });
  }

  // t("Deleted {n} torrents", {n: 5})
  function t(str, vars) {
    if (str == null) return str;
    var hit = dict[str];
    return interpolate(typeof hit === "string" && hit !== "" ? hit : str, vars);
  }

  // Plural forms via the platform. No dependency, and it is right for locales
  // where "one/other" does not match English, such as Russian.
  function tp(n, one, other, vars) {
    var form = "other";
    try {
      form = new Intl.PluralRules(current).select(n);
    } catch (e) {
      form = n === 1 ? "one" : "other";
    }
    var key = form === "one" ? one : other;
    var merged = { n: n };
    if (vars) for (var k in vars) if (Object.prototype.hasOwnProperty.call(vars, k)) merged[k] = vars[k];
    return t(key, merged);
  }

  var ATTRS = ["title", "placeholder", "aria-label", "alt"];
  var SKIP_TAGS = { SCRIPT: 1, STYLE: 1, CODE: 1, PRE: 1, TEXTAREA: 1 };

  function translateDOM(root) {
    root = root || document.body;
    if (!root) return;

    var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
      acceptNode: function (node) {
        if (node.parentNode && SKIP_TAGS[node.parentNode.nodeName]) return NodeFilter.FILTER_REJECT;
        return node.nodeValue && node.nodeValue.trim() ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT;
      }
    });
    var texts = [], n;
    while ((n = walker.nextNode())) texts.push(n);
    for (var i = 0; i < texts.length; i++) {
      var node = texts[i];
      var raw = node.nodeValue;
      var trimmed = raw.trim();
      var hit = dict[trimmed];
      // Only touch strings we actually know: never guess at dynamic content.
      if (typeof hit === "string" && hit !== "") {
        node.nodeValue = raw.replace(trimmed, hit);
      }
    }

    var all = root.querySelectorAll("*");
    for (var k = 0; k < all.length; k++) {
      for (var a = 0; a < ATTRS.length; a++) {
        var name = ATTRS[a];
        if (!all[k].hasAttribute(name)) continue;
        var val = all[k].getAttribute(name).trim();
        var tr = dict[val];
        if (typeof tr === "string" && tr !== "") all[k].setAttribute(name, tr);
      }
    }
  }

  function applyLangAttr() {
    if (document.documentElement) document.documentElement.setAttribute("lang", current);
  }

  function load(code) {
    code = supported(code) || FALLBACK;
    current = code;
    applyLangAttr();
    if (code === FALLBACK) {
      dict = {};
      return Promise.resolve();
    }
    return fetch("/static/i18n/" + code + ".json", { cache: "no-cache" })
      .then(function (r) { return r.ok ? r.json() : {}; })
      .then(function (d) { dict = d || {}; })
      .catch(function () { dict = {}; });   // a missing file must not break the UI
  }

  function setLang(code) {
    try { localStorage.setItem(STORAGE_KEY, code); } catch (e) {}
    return load(code).then(function () {
      translateDOM(document.body);
      try {
        global.dispatchEvent(new CustomEvent("hydra:langchange", { detail: { lang: current } }));
      } catch (e) {}
      return current;
    });
  }

  global.I18N = {
    t: t,
    tp: tp,
    load: load,
    setLang: setLang,
    translateDOM: translateDOM,
    languages: LANGUAGES,
    current: function () { return current; },
    detect: detect
  };
  global.t = t;      // shorthand, app.js calls this a lot
  global.tp = tp;
})(window);
