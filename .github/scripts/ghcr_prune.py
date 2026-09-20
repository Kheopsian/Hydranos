#!/usr/bin/env python3
"""Prune GHCR versions outside the retention window declared in SECURITY.md.

  python3 ghcr-prune.py            -> plan seul, ne supprime rien
  python3 ghcr-prune.py --apply    -> supprime par lots, verifie entre chaque

Pas d action tierce: chaque appel est explicite et verifiable.
"""
import json, os, sys, time, urllib.request, urllib.error

OWNER = os.environ.get("OWNER", "Kheopsian")
PKG   = os.environ.get("PKG", "hydranos")
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

# ---- enfants des index conserves ----------------------------------------
# Un tag ne designe QUE l index. Sur une image multi-architecture les
# manifestes par plateforme et l attestation sont des versions SANS TAG, et la
# version precedente les epargnait toutes en bloc:
#
#     if not tags: spared_untagged += 1; continue
#
# Resultat, la purge retirait l index taggé d une vieille release et laissait
# ses trois ou quatre enfants derriere, orphelins et invisibles. Le nombre de
# tags descendait a KEEP_N pendant que les octets, eux, ne bougeaient pas: le
# registre ne cessait jamais de grossir. C est pour ca qu il y en avait des
# centaines.
#
# Un enfant se supprime avec son parent; un enfant d index CONSERVE ne se
# touche jamais. Il faut donc resoudre les index gardes pour connaitre les
# digests a proteger.
def children_of(ref):
    """Digests des manifestes references par un index, [] si ce n en est pas un."""
    try:
        tok = json.load(urllib.request.urlopen(
            "https://ghcr.io/token?scope=repository:%s/%s:pull&service=ghcr.io"
            % (OWNER.lower(), PKG)))["token"]
        acc = ("application/vnd.oci.image.index.v1+json,"
               "application/vnd.docker.distribution.manifest.list.v2+json,"
               "application/vnd.oci.image.manifest.v1+json")
        req = urllib.request.Request(
            "https://ghcr.io/v2/%s/%s/manifests/%s" % (OWNER.lower(), PKG, ref))
        req.add_header("Authorization", "Bearer " + tok)
        req.add_header("Accept", acc)
        idx = json.loads(urllib.request.urlopen(req, timeout=30).read())
        return [m["digest"] for m in idx.get("manifests", [])]
    except Exception:
        # Illisible: on protege. Ne jamais supprimer sur une absence de reponse.
        return None

protected_digests = set()
unreadable = 0
for v in versions:
    tags = v["metadata"]["container"]["tags"]
    kept_here = v["id"] in recent or any(t == "latest" or t in keep for t in tags)
    if not (kept_here and tags):
        continue
    kids = children_of(tags[0])
    if kids is None:
        unreadable += 1
        continue
    protected_digests.update(kids)
print("index conserves resolus : %d digests enfants proteges (%d index illisibles)"
      % (len(protected_digests), unreadable))

doomed, spared_untagged, spared_kept = [], 0, 0
for v in versions:
    tags = v["metadata"]["container"]["tags"]
    if v["id"] in recent:
        spared_kept += 1; continue
    if tags:
        if any(t == "latest" or t in keep for t in tags):
            spared_kept += 1; continue
        doomed.append((v["id"], tags))
        continue
    # Sans tag: un enfant d index vivant, sinon un orphelin.
    if v["name"] in protected_digests:
        spared_untagged += 1; continue
    if unreadable:
        # Un index n a pas pu etre lu: ses enfants sont inconnus et pourraient
        # etre dans ce lot. On ne supprime aucun orphelin ce tour-ci.
        spared_untagged += 1; continue
    doomed.append((v["id"], ["<sans tag: %s>" % v["name"][:19]]))

print("a supprimer : %d | epargnes : %d taggees/recentes + %d sans tag"
      % (len(doomed), spared_kept, spared_untagged))

# ---- garde-fou: aucune version protegee ne doit etre dans la liste -------
protected = {t for t in keep} | {"latest"}
for vid, tags in doomed:
    inter = protected.intersection(tags)
    assert not inter, "REFUS: la version %s porte un tag protege %s" % (vid, inter)

# Et aucun enfant d index conserve. Le garde-fou ci-dessus ne regarde que les
# tags: un manifeste de plateforme n en a pas, donc il passait au travers et
# rien n aurait signale la suppression d un enfant encore servi -- le tag reste
# resolvable jusqu a ce qu un client demande CETTE architecture.
by_id = {v["id"]: v for v in versions}
for vid, tags in doomed:
    dig = by_id[vid]["name"]
    assert dig not in protected_digests, (
        "REFUS: la version %s (%s) est un manifeste d un index conserve" % (vid, dig[:19]))
print("garde-fou OK: aucun tag ni enfant protege dans le plan")
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
