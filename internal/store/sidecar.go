package store

import (
	"database/sql"
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
	Documents      int
	Errors         []string
}

// Empty reports whether there was nothing to migrate.
func (r SidecarReport) Empty() bool {
	return r.TaggedTorrents == 0 && r.RegisteredTags == 0 && !r.GlobalCounter &&
		r.TrackerRows == 0 && r.Documents == 0
}

func (r SidecarReport) String() string {
	return fmt.Sprintf("tagged=%d tags=%d global_counter=%v tracker_rows=%d docs=%d errors=%d",
		r.TaggedTorrents, r.RegisteredTags, r.GlobalCounter, r.TrackerRows, r.Documents, len(r.Errors))
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
			for ih, tags := range m {
				if len(tags) == 0 {
					delete(m, ih) // nothing to carry over
				}
			}
			n, err := s.SetTagsBulk(m)
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

	// --- categories.json / provenance.json: copied in verbatim ------------
	//
	// The JSON encoding is unchanged, so this is a byte-for-byte move: the
	// same document, in a row instead of a file. That also makes a rollback a
	// copy back, which matters for data the user cannot rebuild.
	for _, sc := range []struct{ file, key string }{
		{"categories.json", MetaCategories},
		{"provenance.json", MetaProvenance},
	} {
		path := filepath.Join(dataDir, sc.file)
		data, err := os.ReadFile(path)
		if err != nil {
			continue // absent is the normal case after the first boot
		}
		if len(data) == 0 {
			continue
		}
		if err := s.SetMeta(sc.key, string(data)); err != nil {
			fail(sc.file, err)
			continue
		}
		if err := retire(path); err != nil {
			fail(sc.file+" retire", err)
			continue
		}
		rep.Documents++
	}

	return rep
}

// ReplaceAllTags makes the stored tags exactly match m, in one transaction:
// everything is cleared first, then m is applied. Use it on the rare paths where
// the caller cannot name which torrents changed (deleting a tag everywhere, or
// an import); prefer SetTagsBulk with an explicit hash list otherwise, since
// this one has to touch every tagged row.
func (s *Store) ReplaceAllTags(m map[string][]string) (int, error) {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	tx, err := s.db.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	// The clear and the re-apply are one transaction on purpose: committing the
	// clear on its own would mean a crash at the wrong moment wipes every tag.
	if _, err := tx.Exec(`UPDATE torrents SET tags = '' WHERE tags != ''`); err != nil {
		return 0, fmt.Errorf("clear tags: %w", err)
	}
	n, err := applyTags(tx, m)
	if err != nil {
		return 0, err
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return n, nil
}

// SetTagsBulk writes a batch of tag assignments in one transaction, and returns
// how many rows it touched. An entry for a torrent the store does not know is
// skipped rather than resurrected — the JSON overlay had no referential
// integrity and had accumulated entries for torrents removed long ago, so this
// is also where that debt gets dropped.
//
// An empty tag list clears that torrent's tags, which is how a removal is
// expressed: callers pass the current state of every hash they touched.
func (s *Store) SetTagsBulk(m map[string][]string) (int, error) {
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
	n, err := applyTags(tx, m)
	if err != nil {
		return 0, err
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return n, nil
}

// applyTags writes a tag map inside an open transaction, returning how many
// rows existed to be written.
func applyTags(tx *sql.Tx, m map[string][]string) (int, error) {
	stmt, err := tx.Prepare(`UPDATE torrents SET tags = ? WHERE info_hash = ?`)
	if err != nil {
		return 0, err
	}
	defer stmt.Close()
	n := 0
	for ih, tags := range m {
		res, err := stmt.Exec(encodeTags(tags), ih)
		if err != nil {
			return n, fmt.Errorf("set tags %s: %w", ih, err)
		}
		if aff, _ := res.RowsAffected(); aff > 0 {
			n++
		}
	}
	return n, nil
}
