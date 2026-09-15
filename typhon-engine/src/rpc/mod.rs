pub mod dispatch;
pub mod events;
pub mod types;

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::config::EngineConfig;
use crate::disk::DiskManager;
use crate::torrent::TorrentManager;
use types::{RpcRequest, RpcResponse};

pub async fn serve(
    socket_path: &str,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    config: EngineConfig,
) {
    let config = Arc::new(config);

    // Transport selection. A "tcp://host:port" socket path binds a TCP loopback
    // listener; anything else is a Unix domain socket path (default, unchanged
    // on Linux). Needed for the Windows/macOS port where the Go<->engine IPC
    // path cannot use Unix domain sockets. Wire format (line-delimited JSON-RPC)
    // is identical either way.
    if let Some(addr) = socket_path.strip_prefix("tcp://") {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("[rpc] tcp bind {} failed: {}", addr, e);
                return;
            }
        };
        info!("[rpc] listening on tcp://{}", addr);

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!("[rpc] accept error: {}", e);
                    continue;
                }
            };
            stream.set_nodelay(true).ok();
            let (reader, writer) = stream.into_split();
            tokio::spawn(handle_conn(
                reader,
                writer,
                torrent_mgr.clone(),
                disk_mgr.clone(),
                config.clone(),
            ));
        }
    } else {
        serve_unix(socket_path, torrent_mgr, disk_mgr, config).await;
    }
}

#[cfg(unix)]
async fn serve_unix(
    socket_path: &str,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    config: Arc<EngineConfig>,
) {
    use tokio::net::UnixListener;

    // Remove stale socket
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("[rpc] bind {} failed: {}", socket_path, e);
            return;
        }
    };

    // Make socket world-accessible
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o777)).ok();
    }

    info!("[rpc] listening on {}", socket_path);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("[rpc] accept error: {}", e);
                continue;
            }
        };
        let (reader, writer) = stream.into_split();
        tokio::spawn(handle_conn(
            reader,
            writer,
            torrent_mgr.clone(),
            disk_mgr.clone(),
            config.clone(),
        ));
    }
}

#[cfg(not(unix))]
async fn serve_unix(
    socket_path: &str,
    _torrent_mgr: Arc<TorrentManager>,
    _disk_mgr: Arc<DiskManager>,
    _config: Arc<EngineConfig>,
) {
    // No Unix domain sockets on this platform; the daemon is expected to pass a
    // tcp://host:port endpoint (it defaults to that on Windows).
    tracing::error!(
        "[rpc] unix socket path {} unsupported on this platform; use tcp://host:port",
        socket_path
    );
}

// Per-connection JSON-RPC loop, generic over the transport's read/write halves
// (Unix or TCP). Handles request/reply plus event push after subscribe_events.
async fn handle_conn<Rd, Wr>(
    reader: Rd,
    mut writer: Wr,
    tm: Arc<TorrentManager>,
    dm: Arc<DiskManager>,
    cfg: Arc<EngineConfig>,
) where
    Rd: tokio::io::AsyncRead + Unpin,
    Wr: tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    // None until client calls `subscribe_events` on this connection.
    // Once set, we `select!` on both incoming RPCs and bus events so
    // regular request/reply still works alongside pushed events.
    let mut event_rx: Option<tokio::sync::broadcast::Receiver<events::Event>> = None;

    info!("[rpc] client connected");

    loop {
        // Build the event-recv future conditionally; when not
        // subscribed the branch stays pending forever.
        let ev_recv = async {
            match event_rx.as_mut() {
                Some(rx) => rx.recv().await.ok(),
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            // Incoming RPC request.
            read = reader.read_line(&mut line) => {
                match read {
                    Ok(0) => break, // EOF
                    Ok(_) => {}
                    Err(e) => { warn!("[rpc] read error: {}", e); break; }
                }

                let trimmed = line.trim();
                if trimmed.is_empty() { line.clear(); continue; }

                let request: RpcRequest = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        let err = serde_json::json!({"error": format!("parse error: {}", e)});
                        let mut out = serde_json::to_string(&err).unwrap();
                        out.push('\n');
                        writer.write_all(out.as_bytes()).await.ok();
                        line.clear();
                        continue;
                    }
                };

                // Intercept subscribe_events: attach the receiver and
                // reply {ok:true}. All following events will stream out
                // via the ev_recv branch.
                let result = if request.method == "subscribe_events" {
                    if event_rx.is_none() {
                        event_rx = Some(tm.bus().subscribe());
                    }
                    serde_json::json!({"result": {"subscribed": true}})
                } else {
                    dispatch::dispatch(
                        &request.method,
                        &request.params,
                        &tm, &dm, &cfg,
                    )
                };

                let response = if let Some(id) = request.id {
                    let mut r = result;
                    if let serde_json::Value::Object(ref mut map) = r {
                        map.insert("id".to_string(), serde_json::json!(id));
                    }
                    r
                } else {
                    result
                };

                let mut out = serde_json::to_string(&response).unwrap();
                out.push('\n');
                if writer.write_all(out.as_bytes()).await.is_err() { break; }
                line.clear();
            }
            // Pushed event (only when subscribed).
            // Wire format matches Hydra-Go's ltclient.Event:
            //   {"event":"torrent_added","data":{...}}
            // (Event is Serialize with #[serde(tag="event", content="data")])
            Some(ev) = ev_recv => {
                let mut out = serde_json::to_string(&ev).unwrap();
                out.push('\n');
                if writer.write_all(out.as_bytes()).await.is_err() { break; }
            }
        }
    }

    info!("[rpc] client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// A manager rooted in a directory of its own, so a test never writes into
    /// the crate root -- `cargo test` has already left a stray `hydra.db` there
    /// once.
    fn fixture(tag: &str) -> (Arc<TorrentManager>, Arc<DiskManager>, Arc<EngineConfig>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("typhon-rpc-{tag}-{}", std::process::id()));
        let data = root.join("data");
        let resume = root.join("resume");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let disk = Arc::new(DiskManager::new(100));
        let tm = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            disk.clone(),
        ));
        // Every field carries a serde default, so an empty document is the
        // configuration a fresh install runs with.
        let cfg: EngineConfig = toml::from_str("").expect("EngineConfig defaults");
        (tm, disk, Arc::new(cfg), root)
    }

    /// Drive `handle_conn` over an in-memory duplex and collect what it wrote.
    /// The transport is generic precisely so it can be something other than a
    /// socket; this is the payoff.
    async fn exchange(tag: &str, input: &str) -> Vec<serde_json::Value> {
        let (tm, dm, cfg, root) = fixture(tag);
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (srv_rd, srv_wr) = tokio::io::split(server);

        let task = tokio::spawn(handle_conn(srv_rd, srv_wr, tm, dm, cfg));

        client.write_all(input.as_bytes()).await.unwrap();
        // Half-closing the write side is the EOF that ends the loop.
        client.shutdown().await.unwrap();

        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(&root);

        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?} ({e})")))
            .collect()
    }

    /// Every reply is one line of JSON. A reply split across lines would
    /// desynchronise a line-delimited client for the rest of the connection.
    #[tokio::test]
    async fn a_reply_is_exactly_one_line() {
        let out = exchange("oneline", "{\"id\":1,\"method\":\"subscribe_events\"}\n").await;
        assert_eq!(out.len(), 1);
    }

    /// Malformed input is answered, not dropped: a client that sent garbage
    /// still has a request outstanding and would otherwise hang on it.
    #[tokio::test]
    async fn unparseable_input_gets_an_error_reply_and_the_connection_survives() {
        let out = exchange("parse", "not json at all\n{\"id\":7,\"method\":\"subscribe_events\"}\n").await;
        assert_eq!(out.len(), 2, "the bad line is answered AND the good one after it");
        assert!(
            out[0].get("error").and_then(|e| e.as_str()).unwrap_or_default().contains("parse error"),
            "first reply names the parse failure: {:?}",
            out[0]
        );
        assert_eq!(out[1].get("id").and_then(|v| v.as_i64()), Some(7));
    }

    /// Blank lines are keepalive noise, not requests. Answering them would
    /// send a reply the client never asked for and shift every later id.
    #[tokio::test]
    async fn a_blank_line_is_not_a_request() {
        let out = exchange("blank", "\n\n   \n{\"id\":3,\"method\":\"subscribe_events\"}\n").await;
        assert_eq!(out.len(), 1, "only the real request is answered");
        assert_eq!(out[0].get("id").and_then(|v| v.as_i64()), Some(3));
    }

    /// `subscribe_events` is intercepted before dispatch and acknowledged, so
    /// the client knows the stream is live before events start arriving.
    #[tokio::test]
    async fn subscribing_is_acknowledged() {
        let out = exchange("sub", "{\"id\":42,\"method\":\"subscribe_events\"}\n").await;
        assert_eq!(out[0]["result"]["subscribed"], serde_json::json!(true));
        assert_eq!(out[0].get("id").and_then(|v| v.as_i64()), Some(42));
    }

    /// Subscribing twice must not swap the receiver: the second one would
    /// start at the current head and drop whatever was already queued.
    #[tokio::test]
    async fn subscribing_twice_is_acknowledged_twice_and_keeps_one_receiver() {
        let out = exchange(
            "sub2",
            "{\"id\":1,\"method\":\"subscribe_events\"}\n{\"id\":2,\"method\":\"subscribe_events\"}\n",
        )
        .await;
        assert_eq!(out.len(), 2);
        for (i, r) in out.iter().enumerate() {
            assert_eq!(r["result"]["subscribed"], serde_json::json!(true), "reply {i}");
        }
    }

    /// A notification -- a request with no id -- gets a reply with no id.
    /// Echoing an id the client never sent is worse than sending none.
    #[tokio::test]
    async fn a_request_without_an_id_is_answered_without_one() {
        let out = exchange("noid", "{\"method\":\"subscribe_events\"}\n").await;
        assert_eq!(out.len(), 1);
        assert!(out[0].get("id").is_none(), "no id was sent, so none comes back: {:?}", out[0]);
    }

    /// An unknown method is a reply, never a dropped request or a panic.
    #[tokio::test]
    async fn an_unknown_method_is_answered() {
        let out = exchange("unknown", "{\"id\":5,\"method\":\"no_such_method_at_all\"}\n").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("id").and_then(|v| v.as_i64()), Some(5));
        assert!(out[0].get("error").is_some(), "unknown methods report an error: {:?}", out[0]);
    }

    /// A request missing `method` cannot be dispatched; it is a parse failure
    /// because `method` is the one field with no serde default.
    #[tokio::test]
    async fn a_request_without_a_method_is_a_parse_error() {
        let out = exchange("nometh", "{\"id\":1,\"params\":{}}\n").await;
        assert_eq!(out.len(), 1);
        assert!(out[0].get("error").is_some());
    }

    /// Closing with nothing sent is an ordinary disconnect.
    #[tokio::test]
    async fn an_empty_connection_closes_cleanly() {
        let out = exchange("empty", "").await;
        assert!(out.is_empty());
    }

    /// Requests are answered in the order they arrived: a line-delimited
    /// client matches replies by position when it omits ids.
    #[tokio::test]
    async fn replies_come_back_in_request_order() {
        let out = exchange(
            "order",
            "{\"id\":1,\"method\":\"subscribe_events\"}\n{\"id\":2,\"method\":\"no_such_method\"}\n{\"id\":3,\"method\":\"subscribe_events\"}\n",
        )
        .await;
        let ids: Vec<i64> = out.iter().filter_map(|r| r.get("id")?.as_i64()).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
