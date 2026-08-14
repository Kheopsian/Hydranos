# Hydra on Windows

Native Windows build. This archive contains:

- `hydra.exe` — the app (web UI + API). **This is the only one you run.**
- `hydra-engine.exe` — the BitTorrent engine, started automatically by `hydra.exe`.

## Run

1. Unzip both files into a folder you can write to, e.g. `C:\Hydra` (keep them
   **together** — the app looks for the engine next to itself).
2. Double-click **`hydra.exe`**.

Hydra starts in the background with **no console window**. You will find it in
the notification area (the tray, next to the clock) — that icon is how you open
and stop it.

On first run it writes a `default.toml` and a `data\` folder next to itself and
generates an API key. There is **no password to copy down**: you create the
admin account yourself, in the browser, on the welcome screen.

3. Open the web UI at `http://127.0.0.1:8199` — or double-click the tray icon,
   which does the same thing.

No config file to copy, no `--config` to pass. To change ports or paths later,
edit the `default.toml` it created and restart.

## The tray icon

| Action | Result |
| --- | --- |
| Hover | Version, current download / upload rate, torrent count |
| Double-click | Opens the web UI |
| Right-click → **Open Hydra** | Same |
| Right-click → **Quit Hydra** | **Stops Hydra cleanly** |

**Use "Quit Hydra" to stop it.** That is the path that saves resume data for
every torrent before exiting. Killing `hydra.exe` from Task Manager skips it,
and the next start has to re-check the affected torrents.

## Running it from a terminal

Started from PowerShell or `cmd`, Hydra attaches to *that* window and prints its
banner and log lines there, as any console program would. Nothing is hidden —
the console is simply not created when you don't ask for one.

If you need a console window in a case where there isn't one to attach to (a
shortcut, a scheduler, debugging), start it with `--console`.

Either way every line is also written to **`hydra.log`**, next to the config,
and shown in the UI's **Logs** tab.

## Start on boot / run as a service

Hydra has no window and no console of its own, so a service wrapper such as
[NSSM](https://nssm.cc/) works cleanly:

```
nssm install Hydra "C:\Hydra\hydra.exe"
nssm start Hydra
```

Note that a service runs in its own session, so **the tray icon will not be
visible** — manage it from the web UI in that setup.

For a plain start-on-login, a shortcut to `hydra.exe` in the Startup folder
(`Win+R` → `shell:startup`) is enough, and keeps the tray icon.

## VPN

Hydra does not manage the VPN on Windows — use your VPN client's system-wide or
per-app binding / kill-switch (Mullvad, AirVPN, Proton, etc.). All of Hydra's
traffic then goes through the tunnel like any other app.

## Notes

- **Windows Firewall** may prompt on first listen — allow it on your private
  network so peers can reach you.
- Heap profiling (jemalloc) is Linux-only and absent here; the system allocator
  is used instead. No difference for normal use.
- Full docs: https://github.com/Kheopsian/Hydra/wiki/Windows-Install
