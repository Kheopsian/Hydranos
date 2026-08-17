package ltclient

import "encoding/json"

// Request is a JSON-RPC request sent to hydra-engine.
type Request struct {
	ID     int64           `json:"id"`
	Method string          `json:"method"`
	Params json.RawMessage `json:"params"`
}

// Response is a JSON-RPC response from hydra-engine.
type Response struct {
	ID     int64           `json:"id,omitempty"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  string          `json:"error,omitempty"`
	// Event fields (pushed from engine)
	Event string          `json:"event,omitempty"`
	Data  json.RawMessage `json:"data,omitempty"`
}

// AddTorrentParams for the add_torrent command.
type AddTorrentParams struct {
	TorrentPath string `json:"torrent_path"`
	SavePath    string `json:"save_path"`
	Stopped     bool   `json:"stopped"`
	SeedMode    bool   `json:"seed_mode,omitempty"`
}

// AddTorrentResult from add_torrent.
type AddTorrentResult struct {
	InfoHash string `json:"info_hash"`
	Name     string `json:"name"`
	Error    string `json:"error,omitempty"`
}

// FetchMetadataParams starts a magnet resolution job in the engine.
type FetchMetadataParams struct {
	InfoHash string   `json:"info_hash"`
	Trackers []string `json:"trackers,omitempty"`
	Peers    []string `json:"peers,omitempty"`
	// BindingID picks the network binding (and therefore the fwmark/tunnel)
	// the resolution dials out on. Nil means the engine's first binding.
	BindingID *uint32 `json:"binding_id,omitempty"`
}

// FetchMetadataResult from fetch_metadata. Started is false when a job for this
// info hash was already running.
type FetchMetadataResult struct {
	InfoHash string `json:"info_hash"`
	Started  bool   `json:"started"`
	Error    string `json:"error,omitempty"`
}

// GetMetadataResult polls a resolution job. State is one of
// "resolving", "done", "failed" or "unknown"; Info carries the hex-encoded raw
// info dict on "done".
type GetMetadataResult struct {
	State string `json:"state"`
	Info  string `json:"info,omitempty"`
	Error string `json:"error,omitempty"`
}

// TorrentStatus from get_status or list_torrents.
type TorrentStatus struct {
	InfoHash        string  `json:"info_hash"`
	Name            string  `json:"name"`
	State           string  `json:"state"`
	Progress        float64 `json:"progress"`
	TotalSize       int64   `json:"total_size"`
	MultiFile       bool    `json:"multi_file"`
	TotalDone       int64   `json:"total_done"`
	TotalUpload     int64   `json:"total_upload"`
	TotalDownload   int64   `json:"total_download"`
	UploadRate      int     `json:"upload_rate"`
	DownloadRate    int     `json:"download_rate"`
	NumPeers        int     `json:"num_peers"`
	NumSeeds        int     `json:"num_seeds"`
	ListSeeds       int     `json:"list_seeds"`
	ListPeers       int     `json:"list_peers"`
	SavePath        string  `json:"save_path"`
	AddedTime       int64   `json:"added_time"`
	CompletedTime   int64   `json:"completed_time"`
	NumPieces       int     `json:"num_pieces"`
	PieceLength     int     `json:"piece_length"`
	SeedingTime     int     `json:"seeding_time"`
	ActiveTime      int     `json:"active_time"`
	CurrentTracker  string  `json:"current_tracker"`
	TrackerHost     string  `json:"tracker_host"`
	IsPaused        bool    `json:"is_paused"`
	IsFinished      bool    `json:"is_finished"`
	IsSeeding       bool    `json:"is_seeding"`
	IsAnnounced     bool    `json:"is_announced"`
	TrackerError    bool    `json:"tracker_error"`
	TrackerErrorMsg string  `json:"tracker_error_msg"`
	// Why the torrent sits in state "error" (missing data). Empty otherwise.
	ErrorMsg string `json:"error_msg"`
}

// ListTorrentsResult from list_torrents.
type ListTorrentsResult struct {
	Torrents []TorrentStatus `json:"torrents"`
	Count    int             `json:"count"`
}

// PeerInfo from get_peers.
type PeerInfo struct {
	IP            string  `json:"ip"`
	Port          int     `json:"port"`
	Client        string  `json:"client"`
	DLRate        int64   `json:"dl_rate"`
	ULRate        int64   `json:"ul_rate"`
	TotalDownload int64   `json:"total_download"`
	TotalUpload   int64   `json:"total_upload"`
	Progress      float64 `json:"progress"`
	Flags         string  `json:"flags"`
	NumPieces     int     `json:"num_pieces"`
}

// GetPeersResult from get_peers.
type GetPeersResult struct {
	Peers []PeerInfo `json:"peers"`
}

// SessionStats from get_session_stats.
type SessionStats struct {
	TotalUpload   int64 `json:"total_upload"`
	TotalDownload int64 `json:"total_download"`
	UploadRate    int   `json:"upload_rate"`
	DownloadRate  int   `json:"download_rate"`
	NumTorrents   int   `json:"num_torrents"`
	UnseededPeers int   `json:"unseeded_peers"`
}

// FileInfo from get_files.
type FileInfo struct {
	Path string `json:"path"`
	Size int64  `json:"size"`
}

// GetFilesResult from get_files.
type GetFilesResult struct {
	Files []FileInfo `json:"files"`
}

// Availability from get_availability. HasPieceMap is false for seed-mode
// torrents, which carry no bitfield — the other fields are meaningless then.
type Availability struct {
	HasPieceMap     bool    `json:"has_piece_map"`
	NumPieces       int     `json:"num_pieces"`
	MinAvailability int     `json:"min_availability"`
	MaxAvailability int     `json:"max_availability"`
	AvgAvailability float64 `json:"avg_availability"`
}

// Event pushed from the engine.
type Event struct {
	Type string          `json:"event"`
	Data json.RawMessage `json:"data"`
}

// TorrentCompletedData from torrent_completed event.
type TorrentCompletedData struct {
	InfoHash string `json:"info_hash"`
}

// TorrentErrorData from torrent_error event.
type TorrentErrorData struct {
	InfoHash string `json:"info_hash"`
	Error    string `json:"error"`
}

// TorrentAddedData from torrent_added event (typhon push infra 2026-04-19).
type TorrentAddedData struct {
	InfoHash  string `json:"info_hash"`
	Name      string `json:"name"`
	SavePath  string `json:"save_path"`
	TotalSize int64  `json:"total_size"`
	NumPieces int    `json:"num_pieces"`
	Private   bool   `json:"private"`
	SeedMode  bool   `json:"seed_mode"`
}

// TorrentRemovedData from torrent_removed event.
type TorrentRemovedData struct {
	InfoHash string `json:"info_hash"`
}

// TorrentStatsMini is a single entry in a stats_snapshot event —
// only the dynamic fields (rates, counters, peer counts). Static metadata
// (name, save_path) is owned by the periodic refreshStats + add events.
type TorrentStatsMini struct {
	InfoHash        string `json:"info_hash"`
	Status          uint8  `json:"status"` // 0=Stopped 1=Checking 2=Downloading 3=Seeding
	TotalUploaded   int64  `json:"total_uploaded"`
	TotalDownloaded int64  `json:"total_downloaded"`
	UploadRate      int64  `json:"upload_rate"`
	DownloadRate    int64  `json:"download_rate"`
	PeersConnected  int    `json:"peers_connected"`
	PeersInterested int    `json:"peers_interested"`
}

// StatsSnapshotData from stats_snapshot event — only torrents with
// changed counters since the last tick.
type StatsSnapshotData struct {
	Torrents []TorrentStatsMini `json:"torrents"`
}

// DiagnosticStats from get_diagnostics — deep libtorrent session analysis.
type DiagnosticStats struct {
	Counters     map[string]int64 `json:"counters"`
	PeerAnalysis PeerAnalysis     `json:"peer_analysis"`
	Settings     map[string]int   `json:"settings"`
}

// PeerAnalysis — per-torrent peer state aggregation.
type PeerAnalysis struct {
	TotalPeers                  int   `json:"total_peers"`
	PeersInterested             int   `json:"peers_interested"`
	PeersUnchokedInterested     int   `json:"peers_unchoked_interested"`
	PeersChokedInterested       int   `json:"peers_choked_interested"`
	PeersActivelyUploading      int   `json:"peers_actively_uploading"`
	TorrentsWithInterestedPeers int   `json:"torrents_with_interested_peers"`
	TotalPendingSendBytes       int64 `json:"total_pending_send_bytes"`
}
