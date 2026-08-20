package engine

import "errors"

// Role is an engine's behavioural kind. An agent node hosts an arbitrary set of
// engines (Option A), each addressed by a free-form id but still typed by a
// role: race and hoard are different Go types with different logic. RichEngine
// is the uniform surface the agent's routed add/action/subscribe paths use, so
// they dispatch by engine-id and branch on role instead of the old fixed
// race/hoard pair.
type Role string

const (
	RoleRace  Role = "race"
	RoleHoard Role = "hoard"
)

// RichEngine is the common surface an agent needs from a local engine to serve
// routed adds/actions and event subscriptions. Role-specific operations that a
// role does not support return an error (checked by the caller via Role()).
type RichEngine interface {
	Role() Role
	RawEventHub() *EventHub
	HasTorrent(infoHash string) bool
	AddRouted(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error)
	RemoveRouted(infoHash string, deleteFiles bool) error
	// SetUserPaused records the operator's stop/start intent. Routed rather
	// than served by a thin StopTorrent because the intent is what the slot
	// manager and the next restart read; a bare stop is undone within minutes.
	SetUserPaused(infoHash string, paused bool) error
	VerifyTorrent(infoHash string) error
	SetTorrentCategory(infoHash, category, savePath string) error
}

var (
	_ RichEngine = (*RaceEngine)(nil)
	_ RichEngine = (*HoardEngine)(nil)
)

// ---- RaceEngine ----

func (e *RaceEngine) Role() Role { return RoleRace }

// AddRouted adapts the race add signature (which carries magnet/trackers).
func (e *RaceEngine) AddRouted(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error) {
	return e.AddTorrent(torrentPath, magnetURI, savePath, trackers, category)
}

func (e *RaceEngine) RemoveRouted(infoHash string, deleteFiles bool) error {
	return e.RemoveTorrent(infoHash, deleteFiles)
}

// VerifyTorrent is hoard-only; the race engine does not support re-checking.
func (e *RaceEngine) VerifyTorrent(infoHash string) error {
	return errors.New("verify not supported on race engine")
}

// SetTorrentCategory is hoard-only (in-place move); unsupported on race.
func (e *RaceEngine) SetTorrentCategory(infoHash, category, savePath string) error {
	return errors.New("set-category not supported on race engine")
}

// ---- HoardEngine ----

func (e *HoardEngine) Role() Role { return RoleHoard }

// AddRouted adapts the hoard add signature, ignoring magnet/trackers (a hoard
// seeds existing data — no magnet fetch, no injected trackers).
func (e *HoardEngine) AddRouted(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error) {
	return e.AddTorrent(torrentPath, savePath, category)
}

// RemoveRouted wraps the hoard's void RemoveTorrent into the error-returning
// RichEngine shape (remove-keep-data is "don't delete files").
func (e *HoardEngine) RemoveRouted(infoHash string, deleteFiles bool) error {
	e.RemoveTorrent(infoHash, deleteFiles)
	return nil
}
