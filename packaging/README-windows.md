# Hydra on Windows

This archive contains a native Windows build of Hydra:

- `hydra.exe` — the daemon (web UI + API)
- `hydra-engine.exe` — the Typhon BitTorrent engine (started automatically by `hydra.exe`)
- `default.toml.example` — a starting configuration

## Running

1. Copy `default.toml.example` to `default.toml` and edit `data_dir`, the API
   key, and ports as needed. Use a Windows path for `data_dir`, e.g.
   `data_dir = "C:\\Hydra\\data"`.
2. Keep `hydra.exe` and `hydra-engine.exe` in the same folder (the daemon looks
   for the engine next to itself).
3. Start it:

   ```
   hydra.exe --config default.toml
   ```

4. Open the web UI at `http://127.0.0.1:8199` (or the `api_port` you set).

The daemon talks to the engine over a TCP loopback socket (127.0.0.1) — this is
automatic on Windows; no configuration needed.

## VPN

Hydra does not manage the VPN on Windows. Use your VPN client's system-wide or
per-app binding / kill-switch (Mullvad, AirVPN Eddie, Proton, etc.) — all of
Hydra's traffic then egresses through the tunnel like any other app. The
Linux-only fwmark/SO_MARK routing is disabled on Windows by design.

## Notes

- To run unattended, wrap `hydra.exe` in a Windows service manager such as
  [NSSM](https://nssm.cc/) or `sc.exe`.
- Heap profiling (jemalloc) is Linux-only and absent from this build; the
  system allocator is used instead.
