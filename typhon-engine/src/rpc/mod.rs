pub mod dispatch;
pub mod events;
pub mod types;

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o777)).ok();
    }

    info!("[rpc] listening on {}", socket_path);

    let config = Arc::new(config);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("[rpc] accept error: {}", e);
                continue;
            }
        };

        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let cfg = config.clone();

        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
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
                                event_rx = Some(events::subscribe());
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
        });
    }
}
