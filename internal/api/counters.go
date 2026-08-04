package api

import (
	"log/slog"
	"sync/atomic"

	"github.com/Kheopsian/hydra/internal/store"
)

// logCounterErr reports a counter persistence failure. These are loud on
// purpose: lifetime upload is the one number that cannot be recomputed, so a
// write that did not land must never pass silently.
func logCounterErr(what string, err error) {
	slog.Error("counters: "+what+" failed", "err", err)
}

// Durable home for the lifetime carry-over counters.
//
// The in-memory maps and atomics stay exactly where they were — the bench
// sampler reads them every 5s and must not touch a database to do it. What
// changes is where they are written: into the torrent store when there is one,
// so that absorbing a removed torrent's bytes and dropping its row are a single
// transaction instead of two writes to two files.
//
// A front-only node has no store (it owns no torrents, it aggregates agents),
// so the JSON files remain its fallback. That is not legacy left lying around:
// it is the only correct answer when there is no database to write to.
var durableStore atomic.Pointer[store.Store]

// SetStore hands the API layer the torrent store. Called once at boot by the
// monolith; never called in front-only mode.
func SetStore(s *store.Store) {
	if s != nil {
		durableStore.Store(s)
	}
}

func durable() *store.Store { return durableStore.Load() }

// AbsorbOnRemove folds a removed torrent's lifetime bytes into the global and
// per-tracker carry-overs and drops its row from the store, atomically.
//
// This is the reason the counters moved out of JSON. Previously the absorb and
// the row deletion were two writes to two files with nothing spanning them, so
// a crash in between either double-counted the torrent on the next boot or lost
// its bytes for good — and lifetime upload cannot be recomputed from anything.
//
// The row is dropped here, on the *before*-remove hook, so the durable state
// moves in one step. If the engine's own removal then failed, the store would
// be missing a torrent the engine still holds; the periodic reconcile puts that
// back. That window is self-healing, the one it replaces was not.
func AbsorbOnRemove(engineName, tracker, infoHash string, ul, dl int64) {
	if ul <= 0 && dl <= 0 {
		return
	}
	if tracker == "" {
		tracker = "(none)"
	}

	// In-memory first: these are what every reader in the process sees.
	absorbGlobalMem(engineName, ul, dl)
	absorbTrackerMem(engineName, tracker, ul, dl)

	st := durable()
	if st == nil {
		// Front-only: no database, keep the JSON files honest.
		saveBaseline()
		saveTrackerBaseline()
		return
	}
	keys := []string{
		store.CounterGlobal,
		store.TrackerCounterKey(engineName, tracker),
	}
	if err := st.DeleteAbsorb(infoHash, keys, ul, dl); err != nil {
		// Persisting failed: fall back to the files rather than silently
		// dropping bytes that cannot be recovered.
		logCounterErr("absorb-on-remove", err)
		saveBaseline()
		saveTrackerBaseline()
	}
}

// loadCountersFromStore repopulates the in-memory global and per-tracker
// carry-overs at boot. Returns false when there is no store to read.
func loadCountersFromStore() bool {
	st := durable()
	if st == nil {
		return false
	}
	all, err := st.CountersAll()
	if err != nil {
		logCounterErr("load counters", err)
		return false
	}
	trackerBaselineMu.Lock()
	defer trackerBaselineMu.Unlock()
	for key, v := range all {
		if key == store.CounterGlobal {
			atomic.StoreInt64(&baselineUploaded, v[0])
			atomic.StoreInt64(&baselineDownloaded, v[1])
			continue
		}
		if eng, trk, ok := store.ParseTrackerCounterKey(key); ok {
			k := trackerKey(eng, trk)
			trackerBaselineUL[k] = v[0]
			trackerBaselineDL[k] = v[1]
		}
	}
	return true
}
