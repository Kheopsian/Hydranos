# Hydranos on Windows

Native Windows build. This archive contains:

- `hydranos.exe` - the daemon: web UI, API and the BitTorrent engine, all in
  one process. **This is the one you run.**
- `hydranos-engine.exe` - the engine as a standalone binary, for multi-node
  setups (`--agent-only`). A single-machine install never needs it.
- `default.toml.example` - a starting configuration.

## Run

1. Unzip into a folder you can write to, e.g. `C:\Hydranos`.
2. Copy `default.toml.example` to `default.toml` and edit `data_dir` to point
   somewhere you can write, e.g. `data_dir = "C:/Hydranos/data"`.
3. Start it:

```
hydranos.exe --config C:\Hydranos\default.toml
```

An API key is generated into that file on first start, and the `data_dir` is
created if it does not exist.

4. Open the web UI at `http://127.0.0.1:8199` (or whatever `api_port` you set).

## What is NOT in this build

The 3.x Windows package was a different program -- a Go daemon with a
notification-area icon and a separate updater. The V4 rewrite is one Rust
binary, and these have **not** been ported:

| Gone | What to do instead |
| --- | --- |
| Tray icon (open / quit / update) | Run it from a terminal, or as a service |
| `hydranos-update.exe` | Download the new archive and replace the `.exe` |
| Starting with no console window | It is a console program; a service wrapper hides it |
| Config written automatically on first run | Copy `default.toml.example` yourself |

⚠ **Stopping it cleanly matters.** Ctrl+C in its console flushes resume data for
every torrent before exiting. Killing it from Task Manager skips that, and the
next start has to re-check the affected torrents.

## Start on boot / run as a service

A service wrapper such as [NSSM](https://nssm.cc/) works:

```
nssm install Hydranos "C:\Hydranos\hydranos.exe" "--config C:\Hydranos\default.toml"
nssm start Hydranos
```

For a plain start-on-login, put a shortcut in the Startup folder
(`Win+R` -> `shell:startup`).

## Updating

Stop Hydranos, unzip the new archive **over the old files**, start it again.

Your settings and data are never touched: `default.toml` and `data\` are not
part of the archive. Unzipping a new release into a *different* folder is what
leaves people wondering where their torrents went.

Verify the download against the `.sha256` published beside it:

```
(Get-FileHash hydranos-<version>-windows-amd64.zip -Algorithm SHA256).Hash
```

## VPN

Hydranos does not manage the VPN on Windows, and the Linux mechanism does not
exist here: interface binding is `SO_BINDTODEVICE`, which Windows has no
equivalent for. Use your VPN client system-wide or per-app
(Mullvad, AirVPN, Proton...). All traffic then goes through the tunnel like any
other application.

⚠ For the same reason the **exit-IP probe reports nothing on Windows** rather
than guess. A probe that answered with the default route address would read as
"the VPN is up" when it is not.

## Notes

- **Windows Firewall** may prompt on first listen -- allow it on your private
  network so peers can reach you.
- **uTP** needs its UDP port free. If another program holds it, uTP is disabled
  with a log line and TCP carries on alone.
- Heap profiling (jemalloc) is Linux-only and absent here; the system allocator
  is used instead. No difference for normal use.
- Full docs: https://github.com/Kheopsian/Hydranos/wiki
