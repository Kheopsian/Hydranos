package store

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// One-shot import of the JSON sidecars that used to sit next to the database.
//
// This runs once on every existing installation, unattended, against data the
// user cannot reconstruct — so it is deliberately conservative: a sidecar is
// only renamed aside once its contents are committed, nothing is ever deleted,
// and every write is an overwrite rather than an increment so a re-run after a
// half-finished upgrade lands on the same numbers instead of doubling them.

// SidecarReport says what the import found, for the boot log.
type SidecarReport struct {
	TaggedTorrents int
	RegisteredTags int
	GlobalCounter  bool
	TrackerRows    int
	Errors         []string
}

// Empty reports whether there was nothing to migrate.
func (r SidecarReport) Empty() bool {
	return r.TaggedTorrents == 0 && r.RegisteredTags == 0 && !r.GlobalCounter && r.TrackerRows == 0
}

func (r SidecarReport) String() string {
	return fmt.Sprintf("tagged=%d tags=%d global_counter=%v tracker_rows=%d errors=%d",
		r.TaggedTorrents, r.RegisteredTags, r.GlobalCounter, r.TrackerRows, len(r.Errors))
}

// retire moves a fully-imported sidecar out of the way. It is kept, not
// removed: if the import turns out to be wrong, the original numbers are still
// on disk next to the database.
func retire(path string) error {
	return os.Rename(path, path+".migrated")
}

// MigrateSidecars imports tags.json, tags_registry.json, baseline.json and
// baseline_trackers.json into the store, then renames each one aside. Absent
// files are not an error — that is the normal case for a fresh install and for
// every boot after the first one.
//
// (qbit_baseline.json is deliberately not handled: no code has read or written
// it for several versions, so it is a leftover, not state.)
func MigrateSidecars(dataDir string, s *Store) SidecarReport {
	var rep SidecarReport
	fail := func(what string, err error) {
		rep.Errors = append(rep.Errors, fmt.Sprintf("%s: %v", what, err))
	}

	// --- tags.json: info_hash -> []tag ------------------------------------
	tagsPath := filepath.Join(dataDir, "tags.json")
	if data, err := os.ReadFile(tagsPath); err == nil {
		var m map[string][]string
		if err := json.Unmarshal(data, &m); err != nil {
			fail("tags.json parse", err)
		} else {
			n, err := s.importTags(m)
			rep.TaggedTorrents = n
			if err != nil {
				fail("tags.json import", err)
			} else if err := retire(tagsPath); err != nil {
				fail("tags.json retire", err)
			}
		}
	} else if !os.IsNotExist(err) {
		fail("tags.json read", err)
	}

	// --- tags_registry.json: []name ---------------------------------------
	regPath := filepath.Join(dataDir, "tags_registry.json")
	if data, err := os.ReadFile(regPath); err == nil {
		var names []string
		if err := json.Unmarshal(data, &names); err != nil {
			fail("tags_registry.json parse", err)
		} else {
			if err := s.AddTagNames(names); err != nil {
				fail("tags_registry.json import", err)
			} else {
				rep.RegisteredTags = len(names)
				if err := retire(regPath); err != nil {
					fail("tags_registry.json retire", err)
				}
			}
		}
	} else if !os.IsNotExist(err) {
		fail("tags_registry.json read", err)
	}

	// --- baseline.json: the global lifetime carry-over ---------------------
	blPath := filepath.Join(dataDir, "baseline.json")
	if data, err := os.ReadFile(blPath); err == nil {
		var bl struct {
			TotalUploaded   int64 `json:"total_uploaded"`
			TotalDownloaded int64 `json:"total_downloaded"`
		}
		if err := json.Unmarshal(data, &bl); err != nil {
			fail("baseline.json parse", err)
		} else {
			if err := s.CounterSet(CounterGlobal, bl.TotalUploaded, bl.TotalDownloaded); err != nil {
				fail("baseline.json import", err)
			} else {
				rep.GlobalCounter = true
				if err := retire(blPath); err != nil {
					fail("baseline.json retire", err)
				}
			}
		}
	} else if !os.IsNotExist(err) {
		fail("baseline.json read", err)
	}

	// --- baseline_trackers.json: per (engine, tracker) carry-over ----------
	tbPath := filepath.Join(dataDir, "baseline_trackers.json")
	if data, err := os.ReadFile(tbPath); err == nil {
		var entries []struct {
			Engine  string `json:"engine"`
			Tracker string `json:"tracker"`
			UL      int64  `json:"ul"`
			DL      int64  `json:"dl"`
		}
		if err := json.Unmarshal(data, &entries); err != nil {
			fail("baseline_trackers.json parse", err)
		} else {
			ok := true
			for _, e := range entries {
				if err := s.CounterSet(TrackerCounterKey(e.Engine, e.Tracker), e.UL, e.DL); err != nil {
					fail("baseline_trackers.json import", err)
					ok = false
					break
				}
				rep.TrackerRows++
			}
			if ok {
				if err := retire(tbPath); err != nil {
					fail("baseline_trackers.json retire", err)
				}
			}
		}
	} else if !os.IsNotExist(err) {
		fail("baseline_trackers.json read", err)
	}

	return rep
}

// importTags writes a whole tags.json in one transaction. Torrents the store
// does not know about are skipped rather than resurrected — the JSON overlay
// had no referential integrity, so it accumulated entries for torrents removed
// long ago, and this is where that debt gets dropped.
func (s *Store) importTags(m map[string][]string) (int, error) {
	if len(m) == 0 {
		return 0, nil
	}
	s.wmux.Lock()
	defer s.wmux.Unlock()
	tx, err := s.db.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	stmt, err := tx.Prepare(`UPDATE torrents SET tags = ? WHERE info_hash = ?`)
	if err != nil {
		return 0, err
	}
	defer stmt.Close()
	n := 0
	for ih, tags := range m {
		if len(tags) == 0 {
			continue
		}
		res, err := stmt.Exec(encodeTags(tags), ih)
		if err != nil {
			return n, fmt.Errorf("import tags %s: %w", ih, err)
		}
		if aff, _ := res.RowsAffected(); aff > 0 {
			n++
		}
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return n, nil
}
