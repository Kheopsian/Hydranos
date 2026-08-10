#!/usr/bin/env python3
"""Prune GHCR versions outside the retention window declared in SECURITY.md.

  python3 ghcr-prune.py            -> plan seul, ne supprime rien
  python3 ghcr-prune.py --apply    -> supprime par lots, verifie entre chaque

Pas d action tierce: chaque appel est explicite et verifiable.
"""
import json, os, sys, time, urllib.request, urllib.error

OWNER = os.environ.get("OWNER", "Kheopsian")
PKG   = os.environ.get("PKG", "hydra")
KEEP_N = int(os.environ.get("KEEP") or 10)
BATCH = 20
TOKEN = os.environ["GHCR_TOKEN"]
APPLY = "--apply" in sys.argv
BASE  = "https://api.github.com/users/%s/packages/container/%s" % (OWNER, PKG)

def api(url, method="GET"):
    req = urllib.request.Request(url, method=method)
    req.add_header("Authorization", "Bearer " + TOKEN)
    req.add_header("Accept", "application/vnd.github+json")
    req.add_header("X-GitHub-Api-Version", "2022-11-28")
    for attempt in range(4):
        try:
            r = urllib.request.urlopen(req, timeout=60)
            b = r.read()
            return r.status, (json.loads(b) if b else None)
        except urllib.error.HTTPError as e:
            if e.code < 500 and e.code != 429:
                return e.code, None
            time.sleep(5 * (attempt + 1))
        except Exception:
            time.sleep(5 * (attempt + 1))
    return 0, None

def vkey(t):
    try:    return [int(x) for x in t.lstrip("v").split(".")]
    except Exception: return None

# ---- inventaire ----------------------------------------------------------
versions, page = [], 1
while True:
    st, batch = api("%s/versions?per_page=100&page=%d" % (BASE, page))
    if st != 200 or not batch:
        break
    versions += batch
    page += 1
print("versions dans le registre :", len(versions))

rel_tags = sorted({t for v in versions for t in v["metadata"]["container"]["tags"] if vkey(t)}, key=vkey)
keep = set(rel_tags[-KEEP_N:])
print("tags de version : %d | conserves : %s" % (len(rel_tags), sorted(keep, key=vkey)))

# filet: les 15 versions les plus recentes sont intouchables quoi qu il arrive
recent = {v["id"] for v in sorted(versions, key=lambda x: x["created_at"], reverse=True)[:15]}

doomed, spared_untagged, spared_kept = [], 0, 0
for v in versions:
    tags = v["metadata"]["container"]["tags"]
    if not tags:
        spared_untagged += 1; continue
    if v["id"] in recent or any(t == "latest" or t in keep for t in tags):
        spared_kept += 1; continue
    doomed.append((v["id"], tags))

print("a supprimer : %d | epargnes : %d taggees + %d sans tag" % (len(doomed), spared_kept, spared_untagged))

# ---- garde-fou: aucune version protegee ne doit etre dans la liste -------
protected = {t for t in keep} | {"latest"}
for vid, tags in doomed:
    inter = protected.intersection(tags)
    assert not inter, "REFUS: la version %s porte un tag protege %s" % (vid, inter)
print("garde-fou OK: aucun tag protege dans le plan")
print("echantillon :", [t for _, t in doomed[:6]])

if not APPLY:
    print("\n--- PLAN SEUL, rien supprime. Relancer avec --apply. ---")
    sys.exit(0)

# ---- verification que les tags vivants repondent -------------------------
def live_ok():
    tok = json.load(urllib.request.urlopen(
        "https://ghcr.io/token?scope=repository:%s/%s:pull&service=ghcr.io" % (OWNER.lower(), PKG)))["token"]
    acc = ("application/vnd.oci.image.index.v1+json,"
           "application/vnd.docker.distribution.manifest.list.v2+json,"
           "application/vnd.oci.image.manifest.v1+json")
    for tag in list(keep) + ["latest"]:
        req = urllib.request.Request(
            "https://ghcr.io/v2/%s/%s/manifests/%s" % (OWNER.lower(), PKG, tag))
        req.add_header("Authorization", "Bearer " + tok)
        req.add_header("Accept", acc)
        try:
            idx = json.loads(urllib.request.urlopen(req, timeout=30).read())
        except Exception as e:
            return "index %s KO (%s)" % (tag, e)
        for m in idx.get("manifests", []):
            r2 = urllib.request.Request(
                "https://ghcr.io/v2/%s/%s/manifests/%s" % (OWNER.lower(), PKG, m["digest"]))
            r2.add_header("Authorization", "Bearer " + tok)
            r2.add_header("Accept", acc)
            try: urllib.request.urlopen(r2, timeout=30).read(1)
            except Exception: return "sous-manifest de %s KO" % tag
    return None

bad = live_ok()
if bad:
    print("ARRET avant suppression, etat initial deja casse :", bad); sys.exit(1)
print("etat initial verifie: les 11 tags vivants repondent\n")

deleted = 0
for i in range(0, len(doomed), BATCH):
    lot = doomed[i:i + BATCH]
    for vid, tags in lot:
        st, _ = api("%s/versions/%d" % (BASE, vid), method="DELETE")
        if st not in (204, 404):
            print("  echec suppression %s (HTTP %s) tags=%s" % (vid, st, tags))
            print("  ARRET. %d supprimees avant l echec." % deleted); sys.exit(1)
        deleted += 1
    bad = live_ok()
    if bad:
        print("  ARRET: un tag vivant a casse apres %d suppressions -> %s" % (deleted, bad)); sys.exit(1)
    print("  %d/%d supprimees, tags vivants OK" % (deleted, len(doomed)), flush=True)

print("\nTermine: %d versions supprimees, les 11 tags vivants intacts." % deleted)
