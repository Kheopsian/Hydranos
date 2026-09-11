#!/usr/bin/env python3
"""Reclaim payload that Hydranos holds more than once.

Two torrents whose `pieces` are byte-identical hold the same payload: the piece
hashes cover the data stream and no name of any kind. When such torrents sit at
different save paths, every copy after the first is disk spent on bytes we
already have -- and a hardlink gives them back without touching what seeds.

Dry run by default. Nothing is unlinked until --apply is passed, and even then
every removal is preceded by a link that is verified to share the source inode.

    python3 dedup_sweep.py                  # report only
    python3 dedup_sweep.py --limit 20       # look at 20 groups
    python3 dedup_sweep.py --apply --limit 20
"""

import argparse, collections, hashlib, os, sqlite3, sys

DB = "file:/mnt/cache/appdata/hydra/hydra.db?mode=ro"

# The daemon sees container paths; this script runs on the host.
MOUNTS = {
    "/data": "/mnt/datapool/data",
    "/calewood": "/mnt/calepool/data",
    "/race": "/mnt/race",
    "/configs": "/mnt/cache/appdata/hydra",
}


def host_path(p):
    for guest, host in MOUNTS.items():
        if p == guest or p.startswith(guest + "/"):
            return host + p[len(guest):]
    return p


# --- just enough bencode to read the info dict ------------------------------

def bfield(blob, key):
    needle = str(len(key)).encode() + b":" + key
    i = blob.find(needle)
    if i < 0:
        return None
    j = i + len(needle)
    c = blob.find(b":", j)
    if c < 0:
        return None
    try:
        ln = int(blob[j:c])
    except ValueError:
        return None
    return blob[c + 1:c + 1 + ln]


def read_int_at(blob, at):
    e = blob.find(b"e", at)
    try:
        return int(blob[at:e])
    except ValueError:
        return None


def read_string_list(blob, at):
    out = []
    while True:
        if at >= len(blob):
            return None, at
        if blob[at:at + 1] == b"e":
            return out, at + 1
        c = blob.find(b":", at)
        try:
            ln = int(blob[at:c])
        except ValueError:
            return None, at
        out.append(blob[c + 1:c + 1 + ln].decode("utf-8", "replace"))
        at = c + 1 + ln


def layout(blob):
    """(name, multi_file, [(relative path, length)]) in stream order."""
    name = bfield(blob, b"name")
    if name is None:
        return None
    name = name.decode("utf-8", "replace")

    if blob.find(b"5:filesl") < 0:
        i = blob.find(b"6:lengthi")
        if i < 0:
            return None
        ln = read_int_at(blob, i + len(b"6:lengthi"))
        return (name, False, [("", ln)]) if ln is not None else None

    files, cur = [], blob.find(b"5:filesl")
    while True:
        i = blob.find(b"6:lengthi", cur)
        if i < 0:
            break
        ln = read_int_at(blob, i + len(b"6:lengthi"))
        p = blob.find(b"4:pathl", i)
        if ln is None or p < 0:
            break
        comps, end = read_string_list(blob, p + len(b"4:pathl"))
        if comps is None:
            break
        files.append(("/".join(comps), ln))
        cur = end
    return (name, True, files) if files else None


def on_disk(save_path, name, multi, rel):
    base = os.path.join(save_path, name)
    return os.path.join(base, rel) if multi else base


def human(n):
    for unit in ("o", "Kio", "Mio", "Gio", "Tio"):
        if abs(n) < 1024 or unit == "Tio":
            return f"{n:.2f} {unit}" if unit != "o" else f"{n} o"
        n /= 1024


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true",
                    help="actually relink (default is a dry run)")
    ap.add_argument("--limit", type=int, default=0, help="only this many groups")
    ap.add_argument("--min-mib", type=int, default=16,
                    help="skip groups whose payload is smaller than this")
    args = ap.parse_args()

    con = sqlite3.connect(DB, uri=True)
    groups = collections.defaultdict(list)
    for ih, sess, sp, blob in con.execute(
            "SELECT info_hash, session, save_path, torrent FROM torrents"):
        p = bfield(blob, b"pieces")
        if not p or len(p) % 20:
            continue
        groups[hashlib.sha1(p).hexdigest()].append((ih, sess, sp, blob))

    # A group is only waste when its members sit at different locations: share
    # a save path and name, and the bytes are already shared.
    todo = []
    for key, members in groups.items():
        if len({m[0] for m in members}) < 2:
            continue
        locs = {(m[2], (bfield(m[3], b"name") or b"").decode("utf-8", "replace")) for m in members}
        if len(locs) < 2:
            continue
        todo.append((key, members))

    print(f"groupes de charge utile identique, stockes a plusieurs endroits : {len(todo)}")
    if args.limit:
        todo = todo[:args.limit]
        print(f"limite a {len(todo)} groupes")
    print("MODE : " + ("APPLICATION REELLE" if args.apply else "simulation (aucune ecriture)"))
    print()

    freed = same_fs = cross_fs = skipped = failed = 0

    for key, members in todo:
        lay = layout(members[0][3])
        if not lay:
            skipped += 1
            continue
        _, multi, files = lay
        total = sum(f[1] for f in files)
        if total < args.min_mib * 1024 * 1024:
            skipped += 1
            continue

        # The oldest complete copy is the source; the others become links to it.
        resolved = []
        for ih, sess, sp, blob in members:
            l2 = layout(blob)
            if not l2:
                continue
            name2, multi2, files2 = l2
            if [f[1] for f in files2] != [f[1] for f in files]:
                continue  # same stream, different cut: pairing by index is unsafe
            paths = [host_path(on_disk(sp, name2, multi2, rel)) for rel, _ in files2]
            ok = all(os.path.isfile(p) and os.path.getsize(p) == ln
                     for p, (_, ln) in zip(paths, files2))
            resolved.append((ih, paths, ok))

        complete = [r for r in resolved if r[2]]
        if len(complete) < 2:
            skipped += 1
            continue

        src_ih, src_paths, _ = complete[0]
        for dst_ih, dst_paths, _ in complete[1:]:
            # Already the same inodes? Then this group costs nothing already.
            if all(os.stat(a).st_ino == os.stat(b).st_ino
                   for a, b in zip(src_paths, dst_paths)):
                continue
            if os.stat(src_paths[0]).st_dev != os.stat(dst_paths[0]).st_dev:
                cross_fs += 1
                continue

            same_fs += 1
            group_bytes = sum(f[1] for f in files)
            if not args.apply:
                freed += group_bytes
                print(f"  [simulation] {dst_ih[:12]} -> {src_ih[:12]}  {human(group_bytes)}")
                continue

            # Link beside the target, verify the inode, then swap. A failure
            # anywhere leaves the original file exactly where it was.
            done = True
            for src, dst in zip(src_paths, dst_paths):
                tmp = dst + ".dedup-tmp"
                try:
                    if os.path.exists(tmp):
                        os.unlink(tmp)
                    os.link(src, tmp)
                    if os.stat(tmp).st_ino != os.stat(src).st_ino:
                        os.unlink(tmp)
                        done = False
                        break
                    os.replace(tmp, dst)
                except OSError as e:
                    print(f"  !! {dst}: {e}")
                    if os.path.exists(tmp):
                        os.unlink(tmp)
                    done = False
                    break
            if done:
                freed += group_bytes
                print(f"  relie {dst_ih[:12]} -> {src_ih[:12]}  {human(group_bytes)}")
            else:
                failed += 1

    print()
    print(f"copies relinkables sur le meme systeme de fichiers : {same_fs}")
    print(f"copies sur un autre systeme de fichiers (EXDEV)    : {cross_fs}")
    print(f"groupes ignores (incomplets, trop petits, decoupe) : {skipped}")
    if failed:
        print(f"echecs                                            : {failed}")
    print(f"espace {'rendu' if args.apply else 'recuperable'} : {human(freed)}")


if __name__ == "__main__":
    sys.exit(main())
