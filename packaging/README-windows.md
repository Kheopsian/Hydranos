# Hydranos on Windows

Native Windows build. This archive contains:

- `hydranos.exe` - the daemon: web UI, API and the BitTorrent engine, all in
  one process. **This is the one you run.**
- `hydranos-update.exe` - the updater. You do not normally run it yourself;
  the tray starts it for you.
- `hydranos-engine.exe` - the engine as a standalone binary, for multi-node
  setups (`--agent-only`). A single-machine install never needs it.
- `default.toml.example` - a starting configuration.

## Run

1. Unzip into a folder you can write to, e.g. `C:\Hydranos`.
2. Double-click **`hydranos.exe`**.

It starts with **no console window** and writes `default.toml`, a `data\`
folder and `hydranos.log` beside itself on first run, generating an API key.
You will find it in the notification area, next to the clock.

3. Open the web UI at `http://127.0.0.1:8199` - or double-click the tray icon.

To change ports or paths, edit the `default.toml` it created and restart.
Pass `--config <path>` to keep it somewhere else.

## The tray icon

| Action | Result |
| --- | --- |
| Hover | Version, torrent count, totals |
| Double-click | Opens the web UI |
| Right-click -> **Open Hydranos** | Same |
| Right-click -> **Check for updates** | Runs the updater in a console |
| Right-click -> **Quit Hydranos** | **Stops it cleanly** |

⚠ **Use "Quit Hydranos" to stop it.** That is the path that saves resume data
for every torrent before exiting. Killing `hydranos.exe` from Task Manager
skips it, and the next start has to re-check the affected torrents.

⚠ A service started by a wrapper runs in its own session and **shows no tray
icon** - manage it from the web UI in that setup.

## Running it from a terminal

Started from PowerShell or `cmd` it attaches to *that* window and prints its
log there. Nothing is hidden; the console is simply not created when you do not
ask for one. Use `--console` to force one (a shortcut, a scheduler, debugging).

Either way every line is also written to **`hydranos.log`**, beside the config.
Setting `HYDRANOS_LOG_STDOUT` stops the file being written at all.

## Updating

Right-click the tray icon and pick **Check for updates**, or run it yourself:

```
hydranos-update.exe --check
hydranos-update.exe --dir C:\Hydranos
```

It reports the latest release, downloads the archive, checks it against the
**SHA-256 published beside it**, stops Hydranos, replaces the binaries and
tells you to start it again. `--tag vX.Y.Z` installs a specific release,
including an older one.

⚠ **If the download or the checksum fails, nothing is replaced and Hydranos is
not stopped.** It also refuses outright when no `.sha256` is published beside
the archive rather than install unverified bytes.

Your settings and data are never touched: `default.toml`, `data\` and
`hydranos.log` are not part of the archive, which is what makes updating in
place the one route that cannot lose them.

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
