package engine

import "strings"

// TorrentFilter is the selection the user has on screen, sent verbatim by the
// UI so that "select all" does not have to ship one hash per torrent.
//
// The rules here MUST match the ones the table applies in the browser: if they
// drift, the user stops a different set than the one they were looking at, and
// nothing says so. That is why the bulk handler answers with the count it
// matched -- the UI compares it against what it displayed.
type TorrentFilter struct {
	Search   string `json:"search"`
	Category string `json:"category"`
	Tracker  string `json:"tracker"`
	Tag      string `json:"tag"`
	State    string `json:"state"`
}

// FilterNone is the sentinel the UI uses for "has no category" / "has no tag".
const FilterNone = "__none__"

// Matches reports whether one torrent survives the filter.
func (f TorrentFilter) Matches(s *TorrentStats) bool {
	if f.Search != "" {
		name := s.Name
		if name == "" {
			name = s.InfoHash
		}
		if !strings.Contains(strings.ToLower(name), strings.ToLower(f.Search)) {
			return false
		}
	}
	switch f.Category {
	case "":
	case FilterNone:
		if s.Category != "" {
			return false
		}
	default:
		if s.Category != f.Category {
			return false
		}
	}
	if f.Tracker != "" && s.TrackerHost != f.Tracker {
		return false
	}
	switch f.Tag {
	case "":
	case FilterNone:
		if len(s.Tags) > 0 {
			return false
		}
	default:
		found := false
		for _, t := range s.Tags {
			if t == f.Tag {
				found = true
				break
			}
		}
		if !found {
			return false
		}
	}
	switch f.State {
	case "":
	case "__active__":
		if s.State != "seeding" || s.UploadRate <= 0 {
			return false
		}
	case "__tracker_err__":
		if !s.TrackerError {
			return false
		}
	case "__error__":
		if !s.TorrentError {
			return false
		}
	default:
		if s.State != f.State {
			return false
		}
	}
	return true
}

// MatchHashes returns the info hashes matching the filter, minus the excluded
// ones. Walks the cached stats under the read lock without copying the list --
// at 100k torrents a copy would cost more than the action itself.
func (e *HoardEngine) MatchHashes(f TorrentFilter, exclude map[string]bool) []string {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	out := make([]string, 0, 64)
	for ih, st := range e.cachedStats {
		if exclude[ih] {
			continue
		}
		if f.Matches(st) {
			out = append(out, ih)
		}
	}
	return out
}

// MatchHashes is the race engine's copy -- see the hoard version above.
func (e *RaceEngine) MatchHashes(f TorrentFilter, exclude map[string]bool) []string {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	out := make([]string, 0, 64)
	for ih, st := range e.cachedStats {
		if exclude[ih] {
			continue
		}
		if f.Matches(st) {
			out = append(out, ih)
		}
	}
	return out
}
