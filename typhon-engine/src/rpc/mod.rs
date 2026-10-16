//! What is left of the engine's JSON-RPC layer: the JSON a torrent reads out
//! as, and the live event bus. The socket server and the standalone binary
//! that served it are gone; the daemon runs the engine in-process.
pub mod dispatch;
pub mod events;
