#!/usr/bin/env python3
"""Differential oracle for the WRITE half of the API.

Reading paritydiff.py first is worth it: this is its counterpart, and the
difference is the whole point.

For a GET, the response IS the behaviour. For a POST it is not: a mutation that
answers "Ok." while writing something else, or nothing, passes a response
comparison and breaks the library. So every scenario here is judged on two
things:

  1. the response of each step, compared as in paritydiff, and
  2. the STATE OF THE STORE afterwards, dumped table by table and compared row
     by row.

Scenarios run as an ordered sequence against two instances seeded identically,
because mutations compose: setting a category then reading it back is one
behaviour, and testing them apart would miss the pair that matters.

A sequence must also END SOMEWHERE STABLE. 3.x runs a background sync that
copies live engine state back into the store, so a scenario that leaves the
library paused is racing that loop: the comparison then sees paused=1 on one
side and paused=0 on the other, and the difference is the sync, not the port.
Every mutation that changes bulk state is followed by the one that undoes it.

SAFETY. Scenarios must not be able to destroy anything:
  * no `deleteFiles=true` / `delete_files=true` -- the bench mounts real paths
    read-only, but a scenario that asks for deletion is a scenario one edit away
    from running against production;
  * mutations are applied to the bench fixture only, on the --internal network,
    with both engines paused.
Both rules are enforced below rather than trusted.

    writediff.py --a URL --b URL --key K --scenarios scenarios.json \
                 --db-a /path/hydra.db --db-b /path/hydra.db
"""

import argparse
import hashlib
import time as _time
import json
import sqlite3
import sys
import urllib.error
import urllib.parse
import urllib.request


# Keys that ask for payload deletion, in either API's spelling.
#
# Matched STRUCTURALLY, by walking the scenario, not by searching the JSON text:
# a text search for "deletefiles=true" misses `"deleteFiles": "true"` because of
# the quotes, and this guard silently accepted a deleting scenario the first
# time it was tested. A guard that cannot be shown to bite is decoration.
DELETION_KEYS = ("deletefiles", "delete_files")
TRUTHY = ("true", "1", "yes", "on")

# Columns whose value legitimately differs between two instances doing the same
# work. Each one is a decision, not a convenience.
VOLATILE_COLUMNS = {
    # Written from time.Now() when the row is created; two processes cannot
    # agree on it to the second.
    "torrents": {"added_time", "completed_time", "seeding_time"},
    "jobs": {"created_at", "updated_at", "id"},
    "counters": set(),
    "meta": set(),
    "tag_registry": set(),
}


def check_safety(scenarios):
    """Refuse a scenario that could delete payload.

    Walks every dict in the structure and looks at the KEY, so the spelling of
    the value -- "true", true, "1" -- cannot slip past.
    """
    def walk(node, trail="scenario"):
        if isinstance(node, dict):
            for key, value in node.items():
                flat = str(key).lower().replace("-", "").replace("_", "")
                asks_deletion = flat in (k.replace("_", "") for k in DELETION_KEYS)
                if asks_deletion and str(value).strip().lower() in TRUTHY:
                    raise SystemExit(
                        "refus: %s.%s demande la suppression des fichiers. "
                        "Le banc ne joue pas de mutation destructrice."
                        % (trail, key)
                    )
                walk(value, "%s.%s" % (trail, key))
        elif isinstance(node, list):
            for index, item in enumerate(node):
                walk(item, "%s[%d]" % (trail, index))

    walk(scenarios)


def request(base, step, key, timeout=20):
    url = base.rstrip("/") + step["path"]
    if step.get("query"):
        url += "?" + urllib.parse.urlencode(step["query"])

    data = None
    headers = {"X-Api-Key": key}
    if step.get("form") is not None:
        data = urllib.parse.urlencode(step["form"]).encode()
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    elif step.get("body") is not None:
        data = json.dumps(step["body"]).encode()
        headers["Content-Type"] = "application/json"

    req = urllib.request.Request(url, data=data, headers=headers,
                                 method=step.get("method", "POST"))
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read().decode("utf-8", "replace")
    except Exception as exc:
        return None, str(exc)


def encode(value):
    """Make one column value comparable and printable.

    A .torrent blob is not JSON, and printing it would drown the report. Its
    digest is enough: two stores holding the same torrent hold the same bytes,
    and a difference in the digest is a difference in the torrent.
    """
    if isinstance(value, (bytes, bytearray)):
        return "blob:%d:%s" % (len(value), hashlib.sha256(value).hexdigest()[:16])
    return value


def dump_store(path):
    """Every row of every table, keyed by table then primary key."""
    con = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
    out = {}
    tables = [r[0] for r in con.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
    for table in tables:
        cols = [c[1] for c in con.execute("PRAGMA table_info(%s)" % table)]
        volatile = VOLATILE_COLUMNS.get(table, set())
        kept = [c for c in cols if c not in volatile]
        rows = con.execute("SELECT %s FROM %s" % (", ".join(kept), table)).fetchall()
        # Sorted: row order is not part of the contract, content is.
        out[table] = sorted(
            json.dumps({k: encode(v) for k, v in zip(kept, r)}, sort_keys=True)
            for r in rows
        )
    con.close()
    return out


# Compteurs que le moteur fait avancer tout seul. La reference seme pendant le
# banc, le candidat tourne avec HYDRA_ENGINE_NET=0 et reste fige sur la valeur
# de reprise: les comparer, c est poser une question qui n a pas de reponse, et
# la difference observee (512 Kio, 480 Kio -- des multiples de la taille de
# bloc) mesure le temps qui passe, pas le portage. Une mutation qui toucherait
# vraiment ces colonnes se verrait ailleurs: le banc rejoue chaque ecriture et
# compare la REPONSE de la route.
COMPTEURS_VIVANTS = ("total_uploaded", "total_downloaded", "seeding_time")


def _sans_compteurs(rows):
    """Les lignes sans les colonnes que le moteur fait avancer tout seul.

    Applique AVANT la comparaison, pas seulement a l affichage: sinon la
    difference est bien detectee, la table signalee, et le rapport n a plus
    aucun champ a montrer -- un probleme qui ne dit pas ce qu il est.
    """
    out = []
    for r in rows:
        try:
            d = json.loads(r) if isinstance(r, str) else dict(r)
        except Exception:
            out.append(r)
            continue
        for c in COMPTEURS_VIVANTS:
            d.pop(c, None)
        out.append(json.dumps(d, sort_keys=True))
    return out


def compare_store(a, b):
    findings = []
    for table in sorted(set(a) | set(b)):
        rows_a = _sans_compteurs(a.get(table, []))
        rows_b = _sans_compteurs(b.get(table, []))
        if rows_a == rows_b:
            continue
        only_a = [r for r in rows_a if r not in rows_b]
        only_b = [r for r in rows_b if r not in rows_a]
        findings.append({
            "table": table,
            "count_a": len(rows_a),
            "count_b": len(rows_b),
            "only_in_a": only_a[:5],
            "only_in_b": only_b[:5],
        })
    return findings



def _cle_ligne(row):
    """La cle qui identifie une ligne des deux cotes, si on la reconnait."""
    for k in ("info_hash", "id", "key"):
        if isinstance(row, dict) and k in row:
            return "%s=%s" % (k, row[k])
    return None


def _pair_rows(only_a, only_b):
    """Apparie les lignes qui portent la meme cle.

    Une ligne presente des deux cotes avec un champ different apparait dans les
    deux listes. Les apparier transforme "deux lignes mysterieuses" en "ce
    champ-la a change", ce que le lecteur peut agir.
    """
    import json as _json
    def charger(rows):
        out = []
        for r in rows:
            try:
                out.append(_json.loads(r) if isinstance(r, str) else r)
            except Exception:
                out.append(r)
        return out

    a, b = charger(only_a), charger(only_b)
    index_b = {}
    for row in b:
        k = _cle_ligne(row)
        if k:
            index_b[k] = row
    paired, restants_a = [], []
    apparies_b = set()
    for row in a:
        k = _cle_ligne(row)
        if k and k in index_b:
            paired.append((k, row, index_b[k]))
            apparies_b.add(k)
        else:
            restants_a.append(row if isinstance(row, str) else _json.dumps(row, sort_keys=True))
    restants_b = [row if isinstance(row, str) else _json.dumps(row, sort_keys=True)
                  for row in b if _cle_ligne(row) not in apparies_b]
    return paired, restants_a, restants_b




def _champs_divergents(a, b):
    """Les cles dont la valeur differe entre deux lignes appariees."""
    if not isinstance(a, dict) or not isinstance(b, dict):
        return []
    return sorted(k for k in set(a) | set(b)
                  if k not in COMPTEURS_VIVANTS and a.get(k) != b.get(k))

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--a", required=True)
    parser.add_argument("--b", required=True)
    parser.add_argument("--key", default="")
    parser.add_argument("--scenarios", required=True)
    parser.add_argument("--db-a", required=True)
    parser.add_argument("--db-b", required=True)
    parser.add_argument("--settle-store", type=float, default=8.0,
                        help="pause before reading the stores, so 3.x's background "
                             "sync has finished writing")
    args = parser.parse_args()

    with open(args.scenarios) as fh:
        scenarios = json.load(fh)
    check_safety(scenarios)

    failures = 0
    for step in scenarios:
        name = step.get("name") or "%s %s" % (step.get("method", "POST"), step["path"])
        status_a, body_a = request(args.a, step, args.key)
        status_b, body_b = request(args.b, step, args.key)

        if status_b is None and status_a is not None:
            print("[todo ] %s  (B: %s)" % (name, body_b[:60]))
            continue

        problems = []
        if status_a != status_b:
            problems.append("statut %s vs %s" % (status_a, status_b))
        if body_a != body_b:
            # Show WHERE they diverge, not just the first 80 characters. Two
            # bodies that differ only past the cut print identically and send
            # you looking for a bug that is not there -- that cost three
            # round trips before this was fixed.
            offset = next((i for i, (x, y) in enumerate(zip(body_a, body_b)) if x != y),
                          min(len(body_a), len(body_b)))
            start = max(0, offset - 40)
            problems.append(
                "corps divergent a l offset %d\n           A: ...%s\n           B: ...%s"
                % (offset, body_a[start:offset + 90], body_b[start:offset + 90]))

        if problems:
            failures += 1
            print("[diff ] %s" % name)
            for p in problems:
                print("         %s" % p)
        else:
            print("[ok   ] %s" % name)

    # Let both sides settle before reading their stores.
    #
    # 3.x runs a background sync that copies live engine state into the store,
    # so a dump taken the instant the last mutation returns catches it
    # mid-write: rows differ, and by the time anyone looks they match again.
    # That cost two investigations. The dump is taken after a pause and then
    # REPEATED -- a difference only counts if it survives, which tells a real
    # divergence apart from a sync still running.
    print("\n--- etat du store apres la sequence (attente de stabilisation)")
    _time.sleep(args.settle_store)
    store_diffs = compare_store(dump_store(args.db_a), dump_store(args.db_b))
    if store_diffs:
        _time.sleep(args.settle_store)
        second = compare_store(dump_store(args.db_a), dump_store(args.db_b))
        if not second:
            print("[ok   ] difference transitoire, disparue apres stabilisation")
            store_diffs = []
        else:
            store_diffs = second
    if not store_diffs:
        print("[ok   ] les deux stores sont identiques")
    else:
        failures += len(store_diffs)
        for finding in store_diffs:
            print("[diff ] table %s: %d lignes en A, %d en B"
                  % (finding["table"], finding["count_a"], finding["count_b"]))
            # Nommer le champ qui diverge plutot qu imprimer une ligne
            # coupee. Une ligne tronquee a 160 caracteres cache justement le
            # champ fautif quand il est en fin d objet, et fait passer une
            # divergence reelle pour un mystere.
            paired, seuls_a, seuls_b = _pair_rows(finding["only_in_a"],
                                                  finding["only_in_b"])
            for key, row_a, row_b in paired:
                champs = _champs_divergents(row_a, row_b)
                print("         %s: %s" % (key, ", ".join(
                    "%s A=%r B=%r" % (c, row_a.get(c), row_b.get(c))
                    for c in champs) or "identiques apres normalisation"))
            for row in seuls_a:
                print("         seulement A: %s" % row[:400])
            for row in seuls_b:
                print("         seulement B: %s" % row[:400])

    print("\n%d probleme(s)" % failures)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
