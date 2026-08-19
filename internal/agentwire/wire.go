// Package agentwire defines the thin JSON envelopes and method-name registry
// shared by the HydraAgent gRPC server (internal/agent) and the remote
// EngineClient (internal/engine/grpcclient). Both sides speak these exact
// shapes so the wire contract stays a drift-free mirror of engine.EngineClient:
// method results are the ltclient return types marshalled verbatim, so only the
// *params* need envelopes here.
package agentwire

import "time"

// Engine session identifiers carried in CallRequest.engine / SubscribeRequest.engine.
const (
	EngineRace  = "race"
	EngineHoard = "hoard"
)

// Method names carried in CallRequest.method. snake_case, one per networked
// EngineClient method (SetEventHandler is local-only; SubscribeEvents maps to
// the Subscribe stream; Close is local).
const (
	MethodPing                 = "ping"
	MethodAddTorrent           = "add_torrent" // covers AddTorrent + AddTorrentWithOptions
	MethodRemoveTorrent        = "remove_torrent"
	MethodStartTorrent         = "start_torrent"
	MethodStopTorrent          = "stop_torrent"
	MethodSetSavePath          = "set_save_path"
	MethodVerifyTorrent        = "verify_torrent"
	MethodGetStatus            = "get_status"
	MethodListTorrents         = "list_torrents"
	MethodGetPeers             = "get_peers"
	MethodGetSessionStat       = "get_session_stats"
	MethodGetFiles             = "get_files"
	MethodGetAvailability      = "get_availability"
	MethodSetEngineOptFlag     = "set_opt_flag"
	MethodEngineOptFlags       = "get_opt_flags"
	MethodGetTrackers          = "get_trackers"
	MethodSetTrackers          = "set_trackers"
	MethodGetDiagnostics       = "get_diagnostics"
	MethodAddPeers             = "add_peers"
	MethodAddRouted            = "add_routed"             // rich placement add: invokes the agent's own Race/Hoard engine
	MethodActionRouted         = "action_routed"          // rich per-torrent op on the agent's own engine
	MethodListEngines          = "list_engines"           // node-level: enumerate the engines this agent hosts
	MethodNodeInfo             = "node_info"              // node-level: exit (public) IP + host interfaces
	MethodGetAnnounceOverrides = "get_announce_overrides" // node-level: read passkey + client-spoof maps
	MethodSetAnnounceOverride  = "set_announce_override"  // node-level: set/clear one announce override
	MethodTrackerSnapshot      = "tracker_snapshot"       // node-level: per-host announce aggregate
	MethodFetchMetadata        = "fetch_metadata"         // magnet: start resolving an info dict from the swarm
	MethodGetMetadata          = "get_metadata"           // magnet: poll a resolution started by fetch_metadata
)

// FetchMetadataParams starts a magnet resolution on the agent's own engine.
type FetchMetadataParams struct {
	InfoHash string   `json:"info_hash"`
	Trackers []string `json:"trackers,omitempty"`
	Peers    []string `json:"peers,omitempty"`
	// BindingID picks which network binding (and so which tunnel) the
	// resolution dials out on. Nil means the engine's first binding -- never
	// the default route, which would expose the node's real address.
	BindingID *uint32 `json:"binding_id,omitempty"`
}

// GetMetadataParams polls a resolution.
type GetMetadataParams struct {
	InfoHash string `json:"info_hash"`
}

// EngineDescriptor identifies one engine an agent hosts (Option A: a node
// hosts an arbitrary set of engines addressed by id, each with a role).
type EngineDescriptor struct {
	ID   string `json:"id"`
	Role string `json:"role"`
}

// ClientSpoofWire mirrors engine.ClientSpoof on the wire (agentwire must not
// import engine).
type ClientSpoofWire struct {
	PeerIDPrefix string `json:"peer_id_prefix"`
	UserAgent    string `json:"user_agent"`
}

// AnnounceOverrides is the reply to MethodGetAnnounceOverrides: an agent's
// per-host announce override maps.
type AnnounceOverrides struct {
	Passkeys map[string]string          `json:"passkeys"`
	Clients  map[string]ClientSpoofWire `json:"clients"`
}

// TrackerStatWire mirrors engine.TrackerStat: the per-host announce aggregate an
// agent exposes so a front can merge every node's trackers into one view.
type TrackerStatWire struct {
	Host         string    `json:"host"`
	Torrents     int       `json:"torrents"`
	OK           bool      `json:"ok"`
	LastError    string    `json:"last_error"`
	LastAnnounce time.Time `json:"last_announce"`
	Announces    int64     `json:"announces"`
	Errors       int64     `json:"errors"`
}

// AnnounceOverrideParams is the params envelope for MethodSetAnnounceOverride.
// Kind selects the map ("passkey" | "client"); an empty value clears that
// host's override (same semantics as the local /api/announce/* endpoints).
type AnnounceOverrideParams struct {
	Kind         string `json:"kind"`
	Host         string `json:"host"`
	Passkey      string `json:"passkey,omitempty"`
	PeerIDPrefix string `json:"peer_id_prefix,omitempty"`
	UserAgent    string `json:"user_agent,omitempty"`
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

// SetTrackersParams carries a whole replacement tracker list, in tiers.
type SetTrackersParams struct {
	InfoHash string     `json:"info_hash"`
	Trackers [][]string `json:"trackers"`
}

// InfoHashParams is the params envelope for single-torrent query/lifecycle calls.
// OptFlagParams carries an engine-side flag toggle. Value is only meaningful
// for the flags that take a number.
type OptFlagParams struct {
	Flag  string `json:"flag"`
	On    bool   `json:"on"`
	Value int64  `json:"value"`
}

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

// NICInfo is one host network interface (non-loopback IPv4).
type NICInfo struct {
	Name string `json:"name"`
	IP   string `json:"ip"`
	Up   bool   `json:"up"`
}

// NodeInfo is the reply to MethodNodeInfo: an agent's egress (public) IP and
// its host interfaces, so a front can show where each agent exits and offer
// interfaces for binding.
type NodeInfo struct {
	PublicIP   string    `json:"public_ip"`
	Interfaces []NICInfo `json:"interfaces"`
}
