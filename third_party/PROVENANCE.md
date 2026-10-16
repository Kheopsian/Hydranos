# Vendored crates: provenance

Both crates are published on crates.io and substituted here through
`[patch.crates-io]` in `typhon-engine/Cargo.toml`. `Cargo.toml.orig` in each
directory is the upstream manifest, untouched. Every local change is listed
below; anything a diff shows beyond this list is not ours.

## librqbit-dht 5.3.1

- Upstream: https://static.crates.io/crates/librqbit-dht/librqbit-dht-5.3.1.crate
- sha256 of the .crate: `c7cc129194337771a86b0399956c4d9bf1cd97c5f24d14a50be38e170f76a54b`
- `Cargo.toml`: `windows-sys` 0.59 under `cfg(windows)`.
- `src/bprotocol.rs`: a KRPC datagram nested deeper than 100 levels is refused before decoding.
- `src/dht.rs`: the queue of nodes to contact is bounded (1024); a node is dropped when it is full.
- `src/dht.rs`: at most 256 `get_peers` requests in flight.
- `src/dht.rs`: per-request tracing spans at debug level instead of error.
- `src/dht.rs`: the UDP reader skips `ConnectionReset` instead of stopping (Windows, error 10054).
- `src/dht.rs`: `SIO_UDP_CONNRESET` off on the DHT socket (Windows).
- `src/dht.rs`: `DhtConfig::bind_device` pins the socket with `SO_BINDTODEVICE`, and fails where that is not supported.

## librqbit-dualstack-sockets 0.7.0

- Upstream: https://static.crates.io/crates/librqbit-dualstack-sockets/librqbit-dualstack-sockets-0.7.0.crate
- sha256 of the .crate: `f7189f1ff66ca75055137a38b062ba7df9691d071b7172e5090997c62eb4b4de`
- `Cargo.toml`: `windows-sys` 0.59 under `cfg(windows)`, `Win32_System_IO` included.
- `src/socket.rs`: `SIO_UDP_CONNRESET` off on new UDP sockets (Windows).

## Checking it

From the repository root:

```sh
mkdir -p /tmp/prov && for p in librqbit-dht-5.3.1 librqbit-dualstack-sockets-0.7.0; do
  curl -fsSL "https://static.crates.io/crates/${p%-*}/$p.crate" -o "/tmp/prov/$p.crate"
  sha256sum "/tmp/prov/$p.crate" && tar xzf "/tmp/prov/$p.crate" -C /tmp/prov
  diff -ru -x target -x .git -x Cargo.lock "/tmp/prov/$p" "third_party/${p%-*}"
done
```

A change to either crate updates this file in the same commit.
