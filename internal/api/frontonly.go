package api

import (
	"encoding/json"
	"errors"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"time"

	"github.com/Kheopsian/hydra/internal/engine"
)

// Front-only mode: a controller node that runs no local Typhon engine and only
// drives remote agents (multi-home placement). To avoid nil-guarding every read
// handler, the front-only api.Server is wired with these empty engine stubs —
// every read returns empty, and a local add is refused with a clear message
// (route via a category whose placement targets a remote agent instead).

var errNoLocalEngine = errors.New("front-only node: no local engine; route via a category placement targeting a remote agent")

// emptyRaceEngine satisfies RaceEngine with empty/no-op behavior.
type emptyRaceEngine struct{}

// NewEmptyRaceEngine returns a no-op RaceEngine for front-only mode.
func NewEmptyRaceEngine() RaceEngine { return emptyRaceEngine{} }

func (emptyRaceEngine) SampleServedInfoHash() string { return "" }

func (emptyRaceEngine) ClearCategoryLabel(string) int                              { return 0 }
func (emptyRaceEngine) SetUserPaused(string, bool) error                           { return nil }
func (emptyRaceEngine) MatchHashes(engine.TorrentFilter, map[string]bool) []string { return nil }
func (emptyRaceEngine) GetAllStatus() []map[string]interface{}                     { return nil }
func (emptyRaceEngine) GetTorrentDetail(string) map[string]interface{}             { return nil }
func (emptyRaceEngine) GetTorrentFileList(string) []map[string]interface{}         { return nil }
func (emptyRaceEngine) GetTorrentAvailability(string) map[string]interface{}       { return nil }
func (emptyRaceEngine) SetEngineOptFlag(string, bool, int64) (map[string]interface{}, error) {
	return nil, errNoLocalEngine
}
func (emptyRaceEngine) InboundAccepted() (int64, error) { return 0, nil }

func (emptyRaceEngine) EngineOptFlags() (map[string]interface{}, error) {
	return nil, errNoLocalEngine
}
func (emptyRaceEngine) GetTorrentStatus(string) map[string]interface{} { return nil }
func (emptyRaceEngine) AddTorrent(string, string, string, []string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyRaceEngine) AddTorrentSeedMode(string, string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyRaceEngine) RemoveTorrent(string, bool) error           { return errNoLocalEngine }
func (emptyRaceEngine) ReannnounceTorrent(string) bool             { return false }
func (emptyRaceEngine) AddTrackerToTorrent(string, string) error   { return errNoLocalEngine }
func (emptyRaceEngine) GetTrackerTiers(string) ([][]string, error) { return nil, errNoLocalEngine }
func (emptyRaceEngine) SetTrackerTiers(string, [][]string) ([][]string, error) {
	return nil, errNoLocalEngine
}
func (emptyRaceEngine) TorrentFilePath(string) (string, bool)      { return "", false }
func (emptyRaceEngine) GetChokingStats() map[string]interface{}    { return map[string]interface{}{} }
func (emptyRaceEngine) GetSessionSettings() map[string]interface{} { return map[string]interface{}{} }
func (emptyRaceEngine) ApplySettings(map[string]interface{}) map[string]interface{} {
	return map[string]interface{}{}
}
func (emptyRaceEngine) SetListenPort(int) error                { return errNoLocalEngine }
func (emptyRaceEngine) ListenPort() int                        { return 0 }
func (emptyRaceEngine) HasTorrent(string) bool                 { return false }
func (emptyRaceEngine) SessionGrabbed() int64                  { return 0 }
func (emptyRaceEngine) AggregateStats() map[string]interface{} { return map[string]interface{}{} }
func (emptyRaceEngine) GetAllStatusJSON() []json.RawMessage    { return nil }
func (emptyRaceEngine) GetSessionTotals() (int64, int64)       { return 0, 0 }

// emptyHoardEngine satisfies HoardEngine with empty/no-op behavior.
type emptyHoardEngine struct{}

// NewEmptyHoardEngine returns a no-op HoardEngine for front-only mode.
func NewEmptyHoardEngine() HoardEngine { return emptyHoardEngine{} }

func (emptyHoardEngine) GetAllStatus() map[string]interface{}                 { return map[string]interface{}{} }
func (emptyHoardEngine) SampleServedInfoHash() string                         { return "" }
func (emptyHoardEngine) GetTorrentList() []map[string]interface{}             { return nil }
func (emptyHoardEngine) GetTorrentListJSON() []json.RawMessage                { return nil }
func (emptyHoardEngine) GetSessionTotals() (int64, int64)                     { return 0, 0 }
func (emptyHoardEngine) GetTorrentDetail(string) map[string]interface{}       { return nil }
func (emptyHoardEngine) GetTorrentFileList(string) []map[string]interface{}   { return nil }
func (emptyHoardEngine) GetTorrentAvailability(string) map[string]interface{} { return nil }
func (emptyHoardEngine) SetEngineOptFlag(string, bool, int64) (map[string]interface{}, error) {
	return nil, errNoLocalEngine
}
func (emptyHoardEngine) InboundAccepted() (int64, error) { return 0, nil }

func (emptyHoardEngine) EngineOptFlags() (map[string]interface{}, error) {
	return nil, errNoLocalEngine
}
func (emptyHoardEngine) AddTorrent(string, string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyHoardEngine) AddTorrentSeedMode(string, string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyHoardEngine) RemoveTorrent(string, bool) error           { return errNoLocalEngine }
func (emptyHoardEngine) ReannnounceTorrent(string) bool             { return false }
func (emptyHoardEngine) AddTrackerToTorrent(string, string) error   { return errNoLocalEngine }
func (emptyHoardEngine) GetTrackerTiers(string) ([][]string, error) { return nil, errNoLocalEngine }
func (emptyHoardEngine) SetTrackerTiers(string, [][]string) ([][]string, error) {
	return nil, errNoLocalEngine
}
func (emptyHoardEngine) TorrentFilePath(string) (string, bool)                      { return "", false }
func (emptyHoardEngine) SetListenPort(int) error                                    { return errNoLocalEngine }
func (emptyHoardEngine) ListenPort() int                                            { return 0 }
func (emptyHoardEngine) HasTorrent(string) bool                                     { return false }
func (emptyHoardEngine) PauseAll() int                                              { return 0 }
func (emptyHoardEngine) SetUserPaused(string, bool) error                           { return nil }
func (emptyHoardEngine) MarkAllUserPaused(bool) int                                 { return 0 }
func (emptyHoardEngine) MatchHashes(engine.TorrentFilter, map[string]bool) []string { return nil }
func (emptyHoardEngine) ResumeAll() int                                             { return 0 }
func (emptyHoardEngine) RestartStuckVerifying() int                                 { return 0 }
func (emptyHoardEngine) VerifyDownloading() int                                     { return 0 }
func (emptyHoardEngine) VerifyTorrent(string) error                                 { return errNoLocalEngine }
func (emptyHoardEngine) SetTorrentCategory(string, string, string) error            { return errNoLocalEngine }
func (emptyHoardEngine) SetCategoryLabel(string, string) error                      { return errNoLocalEngine }
func (emptyHoardEngine) ClearCategoryLabel(string) int                              { return 0 }
func (emptyHoardEngine) GetTags(string) []string                                    { return nil }
func (emptyHoardEngine) GetAllTags() map[string][]string                            { return nil }
func (emptyHoardEngine) SetTags(string, []string) error                             { return errNoLocalEngine }
func (emptyHoardEngine) AddTags(string, []string) error                             { return errNoLocalEngine }
func (emptyHoardEngine) RemoveTags(string, []string) error                          { return errNoLocalEngine }
func (emptyHoardEngine) SetAddedTime(string, time.Time)                             {}
func (emptyHoardEngine) SetCompletedTime(string, time.Time)                         {}
func (emptyHoardEngine) SetContentFolder(string, *bool)                             {}
func (emptyHoardEngine) GetDownloadSlotStatus() engine.DownloadSlotStats {
	return engine.DownloadSlotStats{}
}
func (emptyHoardEngine) SetDownloadSlotsOverride(int) {}
func (emptyHoardEngine) ClearDownloadSlotsOverride()  {}
func (emptyHoardEngine) PinTorrent(string)            {}
func (emptyHoardEngine) UnpinTorrent(string)          {}
func (emptyHoardEngine) PinnedList() []string         { return nil }

// EventHub returns a real hub even though there is no engine behind it. The
// whole web UI is fed over /api/events, and that handler refuses to serve
// without one: a nil hub left a front-only node with a permanently empty
// interface. The snapshot pusher and the agent-row pusher publish into this.
func (emptyHoardEngine) EventHub() *engine.EventHub { return frontOnlyHub }

// frontOnlyHub is process-wide: a front-only node has exactly one empty hoard
// engine, and the stub is a value type with nowhere to keep it.
var frontOnlyHub = engine.NewEventHub(256)

// SetFrontOnly marks this server as a front-only controller: the built-in
// "local" agent is hidden from /api/agents and category placement never
// defaults to "local" (it targets a remote agent instead).
func (s *Server) SetFrontOnly(v bool) { s.frontOnly = v }

// defaultAgent is the placement target for a category with no explicit
// placement. Normally "local"; in front-only mode, the first remote agent.
func (s *Server) defaultAgent() string {
	if !s.frontOnly {
		return "local"
	}
	s.agentsMu.RLock()
	defer s.agentsMu.RUnlock()
	if len(s.remoteAgents) > 0 {
		return s.remoteAgents[0].name
	}
	return "local" // no agents: local stub add returns a clear error
}
func (emptyRaceEngine) FetchMetadata(string, []string, []string, *uint32) (*ltclient.FetchMetadataResult, error) {
	return nil, errNoLocalEngine
}
func (emptyRaceEngine) GetMetadata(string) (*ltclient.GetMetadataResult, error) {
	return nil, errNoLocalEngine
}
func (emptyHoardEngine) FetchMetadata(string, []string, []string, *uint32) (*ltclient.FetchMetadataResult, error) {
	return nil, errNoLocalEngine
}
func (emptyHoardEngine) GetMetadata(string) (*ltclient.GetMetadataResult, error) {
	return nil, errNoLocalEngine
}

// A front-only node hosts no engine, so it can neither give up a torrent nor
// take one on. See errNoLocalEngine.
func (emptyRaceEngine) Role() engine.Role { return engine.RoleRace }
func (emptyRaceEngine) ExportTorrentState(string) (*ltclient.ResumeRecord, error) {
	return nil, errNoLocalEngine
}
func (emptyRaceEngine) AdoptTorrent(*ltclient.ResumeRecord, string) error { return errNoLocalEngine }
func (emptyRaceEngine) ReleaseTorrent(string) error                       { return errNoLocalEngine }

func (emptyHoardEngine) Role() engine.Role { return engine.RoleHoard }
func (emptyHoardEngine) ExportTorrentState(string) (*ltclient.ResumeRecord, error) {
	return nil, errNoLocalEngine
}
func (emptyHoardEngine) AdoptTorrent(*ltclient.ResumeRecord, string) error { return errNoLocalEngine }
func (emptyHoardEngine) ReleaseTorrent(string) error                       { return errNoLocalEngine }
