package engine

import (
	"bytes"
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// TorrentInfo holds metadata for a tracked torrent (persisted alongside engine state).
type TorrentInfo struct {
	InfoHash        string    `json:"info_hash"`
	Name            string    `json:"name"`
	SavePath        string    `json:"save_path"`
	Category        string    `json:"category"`
	AddedTime       time.Time `json:"added_time"`
	CompletedTime   time.Time `json:"completed_time,omitempty"`
	TorrentFilePath string    `json:"torrent_file_path,omitempty"`
	Tags            []string  `json:"tags,omitempty"`
	UserPaused      bool      `json:"user_paused,omitempty"`
	ContentFolder   *bool     `json:"content_folder,omitempty"`
	InjectedPeers   int       `json:"injected_peers,omitempty"`
	InjectionHit    bool      `json:"injection_hit,omitempty"`
}

// TorrentStats contains the stats fields exposed by the qBittorrent-compatible API.
type TorrentStats struct {
	InfoHash        string   `json:"info_hash"`
	Name            string   `json:"name"`
	State           string   `json:"state"`
	Progress        float64  `json:"progress"`
	UploadRate      int64    `json:"upload_rate"`
	DownloadRate    int64    `json:"download_rate"`
	TotalUpload     int64    `json:"total_upload"`
	TotalDownload   int64    `json:"total_download"`
	NumPeers        int      `json:"num_peers"`
	NumSeeds        int      `json:"num_seeds"`
	TotalSize       int64    `json:"total_size"`
	Ratio           float64  `json:"ratio"`
	SavePath        string   `json:"save_path"`
	EngineSavePath  string   `json:"engine_save_path,omitempty"`
	MultiFile       bool     `json:"multi_file"`
	Category        string   `json:"category"`
	AddedTime       int64    `json:"added_time"`
	CompletedTime   int64    `json:"completed_time"`
	SwarmSeeds      int      `json:"swarm_seeds"`
	SwarmLeechers   int      `json:"swarm_leechers"`
	TrackerError    bool     `json:"tracker_error"`
	TrackerErrorMsg string   `json:"tracker_error_msg,omitempty"`
	IsAnnounced     bool     `json:"is_announced,omitempty"`
	TrackerHost     string   `json:"tracker_host,omitempty"`
	Tags            []string `json:"tags,omitempty"`
	UserPaused      bool     `json:"user_paused,omitempty"`
	TorrentError    bool     `json:"torrent_error"`
	TorrentErrorMsg string   `json:"torrent_error_msg,omitempty"`
	ListSeeds       int      `json:"list_seeds"`
	ListPeers       int      `json:"list_peers"`
	TotalDone       int64    `json:"total_done"`
	ContentFolder   *bool    `json:"content_folder,omitempty"`
	Uploader        string   `json:"uploader,omitempty"`
	InjectedPeers   int      `json:"injected_peers,omitempty"`
	InjectionHit    bool     `json:"injection_hit,omitempty"`
}

// PeerInfo describes a single connected peer.
type PeerInfo struct {
	IP                 string   `json:"ip"`
	Port               string   `json:"port"`
	Client             string   `json:"client"`
	DownSpeed          int64    `json:"down_speed"`
	UpSpeed            int64    `json:"up_speed"`
	TotalDownload      int64    `json:"total_download"`
	TotalUpload        int64    `json:"total_upload"`
	Progress           float64  `json:"progress"`
	Flags              []string `json:"flags"`
	NumPieces          int      `json:"num_pieces"`
	ConnectionDuration int      `json:"connection_duration"`
}

// TrackerInfo describes a single tracker for a torrent.
type TrackerInfo struct {
	URL              string `json:"url"`
	Tier             int    `json:"tier"`
	Message          string `json:"message,omitempty"`
	Fails            int    `json:"fails"`
	ScrapeComplete   int    `json:"scrape_complete"`
	ScrapeIncomplete int    `json:"scrape_incomplete"`
}

// TorrentDetail extends TorrentStats with deep inspection data.
type TorrentDetail struct {
	TorrentStats

	Peers    []PeerInfo    `json:"peers"`
	Trackers []TrackerInfo `json:"trackers"`

	PiecesHave  []int `json:"pieces_have"`
	PiecesAvail []int `json:"pieces_avail"`
	NumPieces   int   `json:"num_pieces"`
	PieceLength int   `json:"piece_length"`

	ActiveTime  int `json:"active_time"`
	SeedingTime int `json:"seeding_time"`

	AvgUploadRate   float64 `json:"avg_upload_rate"`
	AvgDownloadRate float64 `json:"avg_download_rate"`

	RatioEfficiency float64  `json:"ratio_efficiency"`
	TimeToFirstPeer *float64 `json:"time_to_first_peer,omitempty"`

	SwarmSeeds    int `json:"swarm_seeds"`
	SwarmLeechers int `json:"swarm_leechers"`

	ConnectionsLimit int `json:"connections_limit"`
	UploadsLimit     int `json:"uploads_limit"`

	Choking map[string]interface{} `json:"choking,omitempty"`
}

// ---------------------------------------------------------------------------
// libtorrent type conversion helpers
// ---------------------------------------------------------------------------

// LtStatusToTorrentStats converts an ltclient TorrentStatus to our TorrentStats.
func LtStatusToTorrentStats(s ltclient.TorrentStatus, category, savePath string, addedTime, completedTime time.Time) TorrentStats {
	progress := s.Progress
	if s.State == "seeding" {
		progress = 1.0
	}

	// The engine says "paused" for anything halted. Without the intent flag
	// here, assume a scheduler hold; the pause paths below rewrite it to
	// "stopped" for torrents the user stopped.
	state := DeriveState(s.State, false)

	var ratio float64
	if s.TotalDownload > 0 {
		ratio = float64(s.TotalUpload) / float64(s.TotalDownload)
	}

	at := addedTime.Unix()
	if at <= 0 && s.AddedTime > 0 {
		at = s.AddedTime
	}

	ct := int64(0)
	if !completedTime.IsZero() {
		ct = completedTime.Unix()
	} else if s.CompletedTime > 0 {
		ct = s.CompletedTime
	}

	if savePath == "" {
		savePath = s.SavePath
	}

	return TorrentStats{
		InfoHash:        s.InfoHash,
		Name:            s.Name,
		State:           state,
		Progress:        progress,
		UploadRate:      int64(s.UploadRate),
		DownloadRate:    int64(s.DownloadRate),
		TotalUpload:     s.TotalUpload,
		TotalDownload:   s.TotalDownload,
		NumPeers:        s.NumPeers,
		NumSeeds:        s.ListSeeds,
		TotalSize:       s.TotalSize,
		Ratio:           ratio,
		SavePath:        savePath,
		EngineSavePath:  s.SavePath,
		MultiFile:       s.MultiFile,
		Category:        category,
		AddedTime:       at,
		CompletedTime:   ct,
		TotalDone:       s.TotalDone,
		ListSeeds:       s.ListSeeds,
		ListPeers:       s.ListPeers,
		SwarmSeeds:      s.ListSeeds,
		SwarmLeechers:   s.ListPeers,
		TrackerError:    s.TrackerError,
		TrackerErrorMsg: s.TrackerErrorMsg,
		TorrentError:    s.State == "error",
		TorrentErrorMsg: s.ErrorMsg,
		IsAnnounced:     s.IsAnnounced,
		TrackerHost:     s.TrackerHost,
	}
}

// normalizeTags trims whitespace, drops empties, and de-dupes a tag list
// (first occurrence wins). Tags are Go-side labels only.
func normalizeTags(tags []string) []string {
	seen := make(map[string]bool, len(tags))
	var out []string
	for _, t := range tags {
		t = strings.TrimSpace(t)
		if t == "" || seen[t] {
			continue
		}
		seen[t] = true
		out = append(out, t)
	}
	return out
}

// LtPeerToPeerInfo converts an ltclient PeerInfo to our PeerInfo.
func LtPeerToPeerInfo(p ltclient.PeerInfo) PeerInfo {
	// Split flags string into individual flag characters
	var flags []string
	for _, c := range p.Flags {
		flags = append(flags, string(c))
	}

	return PeerInfo{
		IP:            p.IP,
		Port:          fmt.Sprintf("%d", p.Port),
		Client:        p.Client,
		DownSpeed:     p.DLRate,
		UpSpeed:       p.ULRate,
		TotalDownload: p.TotalDownload,
		TotalUpload:   p.TotalUpload,
		Progress:      p.Progress,
		Flags:         flags,
		NumPieces:     p.NumPieces,
	}
}

// ---------------------------------------------------------------------------
// Torrent file parsing helpers (unchanged — still needed for save_path logic)
// ---------------------------------------------------------------------------

// infoHashFromTorrentFile extracts the SHA1 info_hash from raw .torrent bytes.
func infoHashFromTorrentFile(data []byte) (string, error) {
	marker := []byte("4:info")
	idx := bytes.Index(data, marker)
	if idx < 0 {
		return "", fmt.Errorf("no info dict found in torrent file")
	}
	start := idx + len(marker)
	if start >= len(data) || data[start] != 'd' {
		return "", fmt.Errorf("info value is not a dict")
	}
	depth := 0
	i := start
	for i < len(data) {
		switch data[i] {
		case 'd', 'l':
			depth++
			i++
		case 'e':
			depth--
			i++
			if depth == 0 {
				h := sha1.Sum(data[start:i])
				return hex.EncodeToString(h[:]), nil
			}
		case 'i':
			end := bytes.IndexByte(data[i:], 'e')
			if end < 0 {
				return "", fmt.Errorf("malformed integer in bencode")
			}
			i += end + 1
		default:
			colonIdx := bytes.IndexByte(data[i:], ':')
			if colonIdx < 0 {
				return "", fmt.Errorf("malformed string in bencode at offset %d", i)
			}
			var strLen int
			if _, err := fmt.Sscanf(string(data[i:i+colonIdx]), "%d", &strLen); err != nil {
				return "", fmt.Errorf("malformed string length: %w", err)
			}
			i += colonIdx + 1 + strLen
		}
	}
	return "", fmt.Errorf("unterminated info dict")
}

// isMultiFileTorrent checks if a torrent contains multiple files.
func isMultiFileTorrent(data []byte) bool {
	infoMarker := []byte("4:infod")
	infoIdx := bytes.Index(data, infoMarker)
	if infoIdx < 0 {
		return false
	}
	searchEnd := infoIdx + 4096
	if searchEnd > len(data) {
		searchEnd = len(data)
	}
	return bytes.Contains(data[infoIdx:searchEnd], []byte("5:filesl"))
}

func nameFromTorrentFile(data []byte) string {
	marker := []byte("4:name")
	idx := bytes.Index(data, marker)
	if idx < 0 {
		return ""
	}
	pos := idx + len(marker)
	if pos >= len(data) {
		return ""
	}
	colonIdx := bytes.IndexByte(data[pos:], ':')
	if colonIdx < 0 {
		return ""
	}
	var strLen int
	if _, err := fmt.Sscanf(string(data[pos:pos+colonIdx]), "%d", &strLen); err != nil {
		return ""
	}
	start := pos + colonIdx + 1
	if start+strLen > len(data) {
		return ""
	}
	return string(data[start : start+strLen])
}

// ToMap converts a TorrentDetail to map[string]interface{} via JSON roundtrip.
func (d *TorrentDetail) ToMap() map[string]interface{} {
	if d == nil {
		return nil
	}
	data, err := json.Marshal(d)
	if err != nil {
		return nil
	}
	var m map[string]interface{}
	if err := json.Unmarshal(data, &m); err != nil {
		return nil
	}
	return m
}

// minStr returns min(a, b) for string length capping.
func minStr(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// DownloadSlotStats exposes the slot manager state for the API.
type DownloadSlotStats struct {
	MaxSlots        int `json:"max_slots"`
	ActiveSlots     int `json:"active_slots"`
	TotalIncomplete int `json:"total_incomplete"`
	ActivityDemoted int `json:"activity_demoted"`
	Cooldown        int `json:"cooldown"`
	Started         int `json:"started"`
	Stopped         int `json:"stopped"`
}

// TorrentMeta mirrors state.TorrentMeta for import without circular dependency.
type TorrentMeta struct {
	SavePath        string
	TorrentFilePath string
	Category        string
	CompletedTime   time.Time
	ContentFolder   *bool
	// UserPaused rides along so the periodic store sync carries the intent even
	// for a torrent whose row does not exist yet — the immediate UPDATE on pause
	// silently touches nothing in that window.
	UserPaused bool
	Tags       []string
}

// Dummy for unused imports
var _ = strings.HasSuffix
