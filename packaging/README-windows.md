# Hydra on Windows

Native Windows build. This archive contains:

- `hydra.exe` — the app (web UI + API). **This is the only one you run.**
- `hydra-engine.exe` — the BitTorrent engine, started automatically by `hydra.exe`.

## Run

1. Unzip both files into a folder you can write to, e.g. `C:\Hydra` (keep them
   **together** — the app looks for the engine next to itself).
2. Double-click **`hydra.exe`** (or run it from a terminal).

That's it. On first run Hydra writes a `default.toml` and a `data\` folder next
to itself, generates an API key and a temporary admin password (shown once in
the console window — note it down), and starts.

3. Open the web UI at `http://127.0.0.1:8199`.

No config file to copy, no `--config` to pass. To change ports or paths later,
edit the `default.toml` it created and restart.

## Run in the background (as a service)

To run without a console window and start on boot, wrap it with a service
manager such as [NSSM](https://nssm.cc/):

```
nssm install Hydra "C:\Hydra\hydra.exe"
nssm start Hydra
```

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
