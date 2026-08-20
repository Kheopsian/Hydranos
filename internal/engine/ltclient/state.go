package ltclient

import (
	"encoding/json"
	"fmt"
)

// Durable per-torrent state, as the engine stores it.
//
// This is the record that used to be one JSON file per torrent under
// `resume/` and now lives in the engine's `state.db`. Hydra never writes it
// directly -- the engine owns its database -- but it does carry one across
// when a torrent moves from one engine to another, which is the whole reason
// these two calls exist.

// ResumeRecord is the wire shape of typhon's ResumeData. Field names must
// match the Rust struct's serde names exactly: this is the record a torrent
// travels on, and a silently dropped field is progression silently lost.
type ResumeRecord struct {
	InfoHash        string     `json:"info_hash"`
	TorrentPath     string     `json:"torrent_path"`
	SavePath        string     `json:"save_path"`
	SeedMode        bool       `json:"seed_mode"`
	Paused          bool       `json:"paused"`
	TotalUploaded   uint64     `json:"total_uploaded"`
	TotalDownloaded uint64     `json:"total_downloaded"`
	AddedTime       int64      `json:"added_time"`
	CompletedTime   int64      `json:"completed_time"`
	Bitfield        string     `json:"bitfield"`
	Trackers        [][]string `json:"trackers"`
}

// ExportState reads back everything the engine durably knows about one
// torrent: where its data is, how much of it is verified, what it has
// uploaded, which trackers it actually announces to.
func (c *Client) ExportState(infoHash string) (*ResumeRecord, error) {
	raw, err := c.call("export_state", map[string]interface{}{
		"info_hash": infoHash,
	})
	if err != nil {
		return nil, err
	}
	var rec ResumeRecord
	if err := json.Unmarshal(raw, &rec); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal state record: %w", err)
	}
	if rec.InfoHash == "" {
		return nil, fmt.Errorf("ltclient: engine returned an empty state record for %s", infoHash)
	}
	return &rec, nil
}

// ImportState makes this engine adopt a torrent exported from another one,
// progression included. The payload files are not touched: the record's
// save_path is taken at face value, so the caller must have put the data
// where it says before calling.
func (c *Client) ImportState(rec *ResumeRecord) (string, error) {
	params := map[string]interface{}{
		"info_hash":        rec.InfoHash,
		"torrent_path":     rec.TorrentPath,
		"save_path":        rec.SavePath,
		"seed_mode":        rec.SeedMode,
		"paused":           rec.Paused,
		"total_uploaded":   rec.TotalUploaded,
		"total_downloaded": rec.TotalDownloaded,
		"added_time":       rec.AddedTime,
		"completed_time":   rec.CompletedTime,
		"bitfield":         rec.Bitfield,
		"trackers":         rec.Trackers,
	}
	if rec.Trackers == nil {
		params["trackers"] = [][]string{}
	}
	raw, err := c.call("import_state", params)
	if err != nil {
		return "", err
	}
	var res struct {
		InfoHash string `json:"info_hash"`
		Name     string `json:"name"`
	}
	if err := json.Unmarshal(raw, &res); err != nil {
		return "", fmt.Errorf("ltclient: unmarshal import result: %w", err)
	}
	return res.InfoHash, nil
}
