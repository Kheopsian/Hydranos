package api

import (
	"encoding/json"
	"errors"
	"time"

	"github.com/Kheopsian/hydra/internal/engine"
)

// Front-only mode: a controller node that runs no local Typhon engine and only
// drives remote agents (multi-home placement). To avoid nil-guarding every read
// handler, the front-only api.Server is wired with these empty engine stubs —
// every read returns empty, and a local add is refused with a clear message
// (route via a category whose placement targets a remote agent instead).

var errNoLocalEngine = errors.New("front-only node: no local engine — route via a category placement targeting a remote agent")

// emptyRaceEngine satisfies RaceEngine with empty/no-op behavior.
type emptyRaceEngine struct{}

// NewEmptyRaceEngine returns a no-op RaceEngine for front-only mode.
func NewEmptyRaceEngine() RaceEngine { return emptyRaceEngine{} }

func (emptyRaceEngine) SetUserPaused(string, bool) error                   { return nil }
func (emptyRaceEngine) GetAllStatus() []map[string]interface{}             { return nil }
func (emptyRaceEngine) GetTorrentDetail(string) map[string]interface{}     { return nil }
func (emptyRaceEngine) GetTorrentFileList(string) []map[string]interface{} { return nil }
func (emptyRaceEngine) GetTorrentStatus(string) map[string]interface{}     { return nil }
func (emptyRaceEngine) AddTorrent(string, string, string, []string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyRaceEngine) AddTorrentSeedMode(string, string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyRaceEngine) RemoveTorrent(string, bool) error           { return errNoLocalEngine }
func (emptyRaceEngine) ReannnounceTorrent(string) bool             { return false }
func (emptyRaceEngine) AddTrackerToTorrent(string, string) error   { return errNoLocalEngine }
func (emptyRaceEngine) GetChokingStats() map[string]interface{}    { return map[string]interface{}{} }
func (emptyRaceEngine) GetSessionSettings() map[string]interface{} { return map[string]interface{}{} }
func (emptyRaceEngine) ApplySettings(map[string]interface{}) map[string]interface{} {
	return map[string]interface{}{}
}
func (emptyRaceEngine) SetListenPort(int)                      {}
func (emptyRaceEngine) HasTorrent(string) bool                 { return false }
func (emptyRaceEngine) SessionGrabbed() int64                  { return 0 }
func (emptyRaceEngine) AggregateStats() map[string]interface{} { return map[string]interface{}{} }
func (emptyRaceEngine) GetAllStatusJSON() []json.RawMessage    { return nil }
func (emptyRaceEngine) GetSessionTotals() (int64, int64)       { return 0, 0 }

// emptyHoardEngine satisfies HoardEngine with empty/no-op behavior.
type emptyHoardEngine struct{}

// NewEmptyHoardEngine returns a no-op HoardEngine for front-only mode.
func NewEmptyHoardEngine() HoardEngine { return emptyHoardEngine{} }

func (emptyHoardEngine) GetAllStatus() map[string]interface{}               { return map[string]interface{}{} }
func (emptyHoardEngine) GetTorrentList() []map[string]interface{}           { return nil }
func (emptyHoardEngine) GetTorrentListJSON() []json.RawMessage              { return nil }
func (emptyHoardEngine) GetSessionTotals() (int64, int64)                   { return 0, 0 }
func (emptyHoardEngine) GetTorrentDetail(string) map[string]interface{}     { return nil }
func (emptyHoardEngine) GetTorrentFileList(string) []map[string]interface{} { return nil }
func (emptyHoardEngine) AddTorrent(string, string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyHoardEngine) AddTorrentSeedMode(string, string, string) (string, error) {
	return "", errNoLocalEngine
}
func (emptyHoardEngine) RemoveTorrent(string, bool) error                { return errNoLocalEngine }
func (emptyHoardEngine) ReannnounceTorrent(string) bool                  { return false }
func (emptyHoardEngine) AddTrackerToTorrent(string, string) error        { return errNoLocalEngine }
func (emptyHoardEngine) SetListenPort(int)                               {}
func (emptyHoardEngine) HasTorrent(string) bool                          { return false }
func (emptyHoardEngine) PauseAll() int                                   { return 0 }
func (emptyHoardEngine) SetUserPaused(string, bool) error                { return nil }
func (emptyHoardEngine) MarkAllUserPaused(bool) int                      { return 0 }
func (emptyHoardEngine) ResumeAll() int                                  { return 0 }
func (emptyHoardEngine) RestartStuckVerifying() int                      { return 0 }
func (emptyHoardEngine) VerifyDownloading() int                          { return 0 }
func (emptyHoardEngine) VerifyTorrent(string) error                      { return errNoLocalEngine }
func (emptyHoardEngine) SetTorrentCategory(string, string, string) error { return errNoLocalEngine }
func (emptyHoardEngine) SetCategoryLabel(string, string) error           { return errNoLocalEngine }
func (emptyHoardEngine) ClearCategoryLabel(string) int                   { return 0 }
func (emptyHoardEngine) GetTags(string) []string                         { return nil }
func (emptyHoardEngine) GetAllTags() map[string][]string                 { return nil }
func (emptyHoardEngine) SetTags(string, []string) error                  { return errNoLocalEngine }
func (emptyHoardEngine) AddTags(string, []string) error                  { return errNoLocalEngine }
func (emptyHoardEngine) RemoveTags(string, []string) error               { return errNoLocalEngine }
func (emptyHoardEngine) SetAddedTime(string, time.Time)                  {}
func (emptyHoardEngine) SetContentFolder(string, *bool)                  {}
func (emptyHoardEngine) GetDownloadSlotStatus() engine.DownloadSlotStats {
	return engine.DownloadSlotStats{}
}
func (emptyHoardEngine) SetDownloadSlotsOverride(int) {}
func (emptyHoardEngine) ClearDownloadSlotsOverride()  {}
func (emptyHoardEngine) PinTorrent(string)            {}
func (emptyHoardEngine) UnpinTorrent(string)          {}
func (emptyHoardEngine) PinnedList() []string         { return nil }
func (emptyHoardEngine) EventHub() *engine.EventHub   { return nil }

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
