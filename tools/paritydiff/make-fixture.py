#!/usr/bin/env python3
"""Build the bench fixture: one engine's real state, and only the files it needs.

A parity bench on an empty database proves nothing -- two empty lists match
whatever the code does. So the bench replays a real engine: the race engine,
chosen because it holds ~520 torrents rather than hoard's 243k.

Only the .torrent files that state.db actually references are copied. The
production uploads directory is 15 GB across 609k files; the referenced subset
is a few megabytes.

Two things this script is careful about, both learned the hard way:

  * The WAL is part of the database. Copying state.db without state.db-wal
    silently drops everything not yet checkpointed -- here that was ALL 520
    rows, and the engine came up reporting zero torrents with no error.
  * cp goes through copy_file_range, which ZFS turns into block cloning; that
    has already wedged a build on this host in unkillable D state. Everything
    here reads and writes bytes.

The fixture carries real tracker passkeys in the `trackers` column. It lives
outside the repository and must stay there.
"""

import os
import shutil
import sqlite3
import sys

PROD = "/mnt/cache/appdata/hydra/race"
PROD_STORE = "/mnt/cache/appdata/hydra/hydra.db"
FIXTURE = "/mnt/cache/appdata/hydra-v4-staging/fixture"
ENGINE = "race"


def copy_bytes(src, dst):
    """Plain read/write copy. shutil.copyfile would use copy_file_range."""
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    with open(src, "rb") as fh_in, open(dst, "wb") as fh_out:
        while True:
            chunk = fh_in.read(1 << 20)
            if not chunk:
                break
            fh_out.write(chunk)


def main():
    engine_dir = os.path.join(FIXTURE, ENGINE)
    if os.path.isdir(FIXTURE):
        shutil.rmtree(FIXTURE)
    os.makedirs(engine_dir)

    # state.db AND its WAL. The -shm is shared memory, rebuilt on open.
    for name in ("state.db", "state.db-wal"):
        src = os.path.join(PROD, name)
        if os.path.exists(src):
            copy_bytes(src, os.path.join(engine_dir, name))
            print("copie %-16s %.1f Mo" % (name, os.path.getsize(src) / 1e6))

    # The resume directory, small and per-torrent.
    resume_src = os.path.join(PROD, "resume")
    if os.path.isdir(resume_src):
        count = 0
        for entry in os.listdir(resume_src):
            src = os.path.join(resume_src, entry)
            if os.path.isfile(src):
                copy_bytes(src, os.path.join(engine_dir, "resume", entry))
                count += 1
        print("copie resume/         %d fichiers" % count)

    # Only the .torrent files this state references.
    conn = sqlite3.connect(os.path.join(engine_dir, "state.db"))
    paths = [row[0] for row in conn.execute(
        "SELECT torrent_path FROM torrent_state WHERE torrent_path <> ''")]
    conn.close()

    copied = missing = 0
    total = 0
    for path in paths:
        # Paths are absolute as the daemon sees them (/configs/...). Map them
        # back onto the host, then into the fixture at the same relative place
        # so the copied state.db keeps working untouched.
        rel = path[len("/configs/"):] if path.startswith("/configs/") else path.lstrip("/")
        src = os.path.join("/mnt/cache/appdata/hydra", rel)
        if not os.path.isfile(src):
            missing += 1
            continue
        copy_bytes(src, os.path.join(FIXTURE, rel))
        total += os.path.getsize(src)
        copied += 1

    print("copie .torrent        %d fichiers (%.1f Mo), %d introuvables sur %d"
          % (copied, total / 1e6, missing, len(paths)))

    # The FRONT store, restricted to this session.
    #
    # Mirroring only the engine's state.db leaves the bench lopsided: the engine
    # knows all 486 torrents while the front store is empty, so both binaries
    # fall back to whatever they do without a store row -- and they fall back
    # differently. 3.x then publishes completed_time = added_time, which is not
    # what it does in production and not something worth matching. Copying the
    # front rows too makes the comparison test the code instead of the fallback.
    if not os.path.exists(PROD_STORE):
        print("pas de store de prod, saute")
        return 0

    src = sqlite3.connect("file:%s?mode=ro" % PROD_STORE, uri=True)
    schema = [r[0] for r in src.execute(
        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL")]

    dst_path = os.path.join(FIXTURE, "hydra.db")
    dst = sqlite3.connect(dst_path)
    for statement in schema:
        dst.execute(statement)

    # user_version is the migration counter, and copying the schema without it
    # produces a database that LOOKS right and that 3.x refuses to open: it
    # replays migration 1, hits "duplicate column name: paused", and comes up
    # with its store disabled. The bench then compares against a crippled
    # reference and every fallback looks like a porting bug. Copy the counter.
    version = list(src.execute("PRAGMA user_version"))[0][0]
    dst.execute("PRAGMA user_version = %d" % version)

    cols = [c[1] for c in src.execute("PRAGMA table_info(torrents)")]
    placeholders = ",".join("?" * len(cols))
    rows = list(src.execute(
        "SELECT %s FROM torrents WHERE session = ?" % ",".join(cols), (ENGINE,)))
    dst.executemany("INSERT OR REPLACE INTO torrents VALUES (%s)" % placeholders, rows)

    # Categories and the rest of the meta table travel with them: a category
    # name in a torrent row that resolves to nothing would be a different kind
    # of empty than production has.
    for table in ("meta", "tag_registry", "counters"):
        try:
            src_rows = list(src.execute("SELECT * FROM %s" % table))
        except sqlite3.Error:
            continue
        if not src_rows:
            continue
        width = ",".join("?" * len(src_rows[0]))
        dst.executemany("INSERT OR REPLACE INTO %s VALUES (%s)" % (table, width), src_rows)

    dst.commit()
    print("copie hydra.db        %d torrents de la session %s, user_version=%d"
          % (len(rows), ENGINE, version))
    src.close()
    dst.close()
    return 0 if copied else 1


if __name__ == "__main__":
    sys.exit(main())
