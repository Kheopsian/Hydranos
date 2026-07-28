// Package agentwire defines the thin JSON envelopes and method-name registry
// shared by the HydraAgent gRPC server (internal/agent) and the remote
// EngineClient (internal/engine/grpcclient). Both sides speak these exact
// shapes so the wire contract stays a drift-free mirror of engine.EngineClient:
// method results are the ltclient return types marshalled verbatim, so only the
// *params* need envelopes here.
package agentwire

// Engine session identifiers carried in CallRequest.engine / SubscribeRequest.engine.
const (
	EngineRace  = "race"
	EngineHoard = "hoard"
)

// Method names carried in CallRequest.method. snake_case, one per networked
// EngineClient method (SetEventHandler is local-only; SubscribeEvents maps to
// the Subscribe stream; Close is local).
const (
	MethodPing           = "ping"
	MethodAddTorrent     = "add_torrent" // covers AddTorrent + AddTorrentWithOptions
	MethodRemoveTorrent  = "remove_torrent"
	MethodStartTorrent   = "start_torrent"
	MethodStopTorrent    = "stop_torrent"
	MethodSetSavePath    = "set_save_path"
	MethodVerifyTorrent  = "verify_torrent"
	MethodGetStatus      = "get_status"
	MethodListTorrents   = "list_torrents"
	MethodGetPeers       = "get_peers"
	MethodGetSessionStat = "get_session_stats"
	MethodGetFiles       = "get_files"
	MethodGetTrackers    = "get_trackers"
	MethodGetDiagnostics = "get_diagnostics"
	MethodAddPeers       = "add_peers"
	MethodAddRouted      = "add_routed"    // rich placement add: invokes the agent's own Race/Hoard engine
	MethodActionRouted   = "action_routed" // rich per-torrent op on the agent's own engine
	MethodListEngines    = "list_engines"  // node-level: enumerate the engines this agent hosts
)

// EngineDescriptor identifies one engine an agent hosts (Option A: a node
// hosts an arbitrary set of engines addressed by id, each with a role).
type EngineDescriptor struct {
	ID   string `json:"id"`
	Role string `json:"role"`
}

// AddParams is the params envelope for MethodAddTorrent. The .torrent bytes are
// shipped inline (the caller's path is not readable on the agent host); the
// agent writes them to a temp file and calls the local AddTorrent.
type AddParams struct {
	Torrent     []byte `json:"torrent"` // raw .torrent bytes (base64 in JSON)
	SavePath    string `json:"save_path"`
	Stopped     bool   `json:"stopped"`
	SeedMode    bool   `json:"seed_mode"`
	WithOptions bool   `json:"with_options"` // true => AddTorrentWithOptions (honour SeedMode)
}

// InfoHashParams is the params envelope for single-torrent query/lifecycle calls.
type InfoHashParams struct {
	InfoHash string `json:"info_hash"`
}

// RemoveParams is the params envelope for MethodRemoveTorrent.
type RemoveParams struct {
	InfoHash string `json:"info_hash"`
	KeepData bool   `json:"keep_data"`
}

// SetSavePathParams is the params envelope for MethodSetSavePath.
type SetSavePathParams struct {
	InfoHash string `json:"info_hash"`
	SavePath string `json:"save_path"`
}

// Peer mirrors the anonymous {IP,Port} struct of EngineClient.AddPeers.
type Peer struct {
	IP   string `json:"ip"`
	Port int    `json:"port"`
}

// AddPeersParams is the params envelope for MethodAddPeers.
type AddPeersParams struct {
	InfoHash string `json:"info_hash"`
	Peers    []Peer `json:"peers"`
}

// AddRoutedParams is the params envelope for MethodAddRouted — a rich add that
// runs through the agent's OWN RaceEngine/HoardEngine (category + announce +
// drain stay on the agent, per "announce 100% agent-side"), unlike the raw
// EngineClient AddTorrent tunnel.
type AddRoutedParams struct {
	Mode     string `json:"mode"`    // "race" | "hoard" (default race)
	Torrent  []byte `json:"torrent"` // raw .torrent bytes
	SavePath string `json:"save_path"`
	Category string `json:"category"`
}

// ActionRoutedParams is the params envelope for MethodActionRouted — a rich
// per-torrent operation run through the agent's OWN engine.
type ActionRoutedParams struct {
	Mode        string `json:"mode"`   // "race" | "hoard"
	Action      string `json:"action"` // reannounce|remove|verify|setcategory
	InfoHash    string `json:"info_hash"`
	DeleteFiles bool   `json:"delete_files,omitempty"`
	Category    string `json:"category,omitempty"`
	SavePath    string `json:"save_path,omitempty"`
}
