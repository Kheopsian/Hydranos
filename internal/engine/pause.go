package engine

import "errors"

// User pause intent.
//
// This is deliberately not the same thing as a torrent being stopped. The
// download slot manager and the disk slot manager both stop and start torrents
// on their own — that is a *hold*, and it is theirs to undo. What lives here is
// the user's decision, and the rule that makes the two composable is short
// enough to keep in your head:
//
//	only a human (or an API call made on their behalf) writes this flag,
//	and nothing automatic ever clears it.
//
// So a torrent can be held by a scheduler and paused by the user at the same
// time without either having to arbitrate: when the scheduler frees its slot it
// calls the resume path, sees the intent, and leaves the torrent alone. The
// displayed state derives from both — intent means "paused", a scheduler hold
// means "queued" — which is also what qBittorrent shows, so nobody has to learn
// a new concept.
//
// The flag is persisted on the torrent's row in the store, in the same
// transaction as the row itself. That is what makes it survive a restart, and
// what stops it from outliving the torrent.

// --- hoard ------------------------------------------------------------------

// SetUserPaused records the user's intent and acts on it: pausing stops the
// torrent, resuming starts it again. The intent is recorded even if the engine
// call fails, because the intent is the durable thing — the next reconcile or
// restart will make the engine agree with it.
func (e *HoardEngine) SetUserPaused(infoHash string, paused bool) error {
	if err := e.markUserPaused(infoHash, paused); err != nil {
		return err
	}
	if paused {
		return e.StopTorrent(infoHash)
	}
	return e.StartTorrent(infoHash)
}

// IsUserPaused reports whether the user has paused this torrent.
func (e *HoardEngine) IsUserPaused(infoHash string) bool {
	e.mu.RLock()
	defer e.mu.RUnlock()
	info, ok := e.torrents[infoHash]
	return ok && info.UserPaused
}

// UserPausedSet returns every torrent the user has paused.
func (e *HoardEngine) UserPausedSet() map[string]bool {
	e.mu.RLock()
	defer e.mu.RUnlock()
	out := map[string]bool{}
	for ih, info := range e.torrents {
		if info.UserPaused {
			out[ih] = true
		}
	}
	return out
}

// RestoreUserPaused re-applies persisted intent at boot. The engine reloads
// every torrent in a running state, so an intent that is not acted on here
// would silently resume seeding — which is the whole failure this feature
// exists to prevent.
func (e *HoardEngine) RestoreUserPaused(byHash map[string]bool) int {
	var toStop []string
	e.mu.Lock()
	for ih, paused := range byHash {
		if !paused {
			continue
		}
		if info, ok := e.torrents[ih]; ok {
			info.UserPaused = true
			toStop = append(toStop, ih)
		}
	}
	e.mu.Unlock()
	e.syncPausedStats(toStop)
	for _, ih := range toStop {
		_ = e.StopTorrent(ih)
	}
	return len(toStop)
}

func (e *HoardEngine) markUserPaused(infoHash string, paused bool) error {
	e.mu.Lock()
	info, ok := e.torrents[infoHash]
	if !ok {
		e.mu.Unlock()
		return errTorrentNotFound
	}
	info.UserPaused = paused
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.UserPaused = paused
	}
	e.cachedStatsMu.Unlock()
	return nil
}

func (e *HoardEngine) syncPausedStats(hashes []string) {
	if len(hashes) == 0 {
		return
	}
	e.cachedStatsMu.Lock()
	for _, ih := range hashes {
		if st, ok := e.cachedStats[ih]; ok {
			st.UserPaused = true
		}
	}
	e.cachedStatsMu.Unlock()
}

// autoStart is the start path every automatic scheduler must use: it honours the
// user's pause intent, so freeing a download or disk slot can never quietly undo
// a human decision. The plain StartTorrent stays the user's own path.
func (e *HoardEngine) autoStart(infoHash string) error {
	if e.IsUserPaused(infoHash) {
		return nil
	}
	if e.client == nil {
		return errEngineNotReady
	}
	return e.client.StartTorrent(infoHash)
}

// MarkAllUserPaused stamps the intent on every torrent of this engine. Backs
// pause-all / resume-all: the gesture is the user's, so it persists like any
// other pause — a restart after "pause everything" must not quietly resume
// everything.
func (e *HoardEngine) MarkAllUserPaused(paused bool) int {
	e.mu.Lock()
	n := 0
	for _, info := range e.torrents {
		info.UserPaused = paused
		n++
	}
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	for _, st := range e.cachedStats {
		st.UserPaused = paused
	}
	e.cachedStatsMu.Unlock()
	return n
}

// --- race -------------------------------------------------------------------

// SetUserPaused records the user's intent and acts on it.
func (e *RaceEngine) SetUserPaused(infoHash string, paused bool) error {
	if err := e.markUserPaused(infoHash, paused); err != nil {
		return err
	}
	if paused {
		return e.StopTorrent(infoHash)
	}
	return e.StartTorrent(infoHash)
}

// IsUserPaused reports whether the user has paused this torrent.
func (e *RaceEngine) IsUserPaused(infoHash string) bool {
	e.mu.RLock()
	defer e.mu.RUnlock()
	info, ok := e.torrents[infoHash]
	return ok && info.UserPaused
}

// UserPausedSet returns every torrent the user has paused.
func (e *RaceEngine) UserPausedSet() map[string]bool {
	e.mu.RLock()
	defer e.mu.RUnlock()
	out := map[string]bool{}
	for ih, info := range e.torrents {
		if info.UserPaused {
			out[ih] = true
		}
	}
	return out
}

// RestoreUserPaused re-applies persisted intent at boot.
func (e *RaceEngine) RestoreUserPaused(byHash map[string]bool) int {
	var toStop []string
	e.mu.Lock()
	for ih, paused := range byHash {
		if !paused {
			continue
		}
		if info, ok := e.torrents[ih]; ok {
			info.UserPaused = true
			toStop = append(toStop, ih)
		}
	}
	e.mu.Unlock()
	e.syncPausedStats(toStop)
	for _, ih := range toStop {
		_ = e.StopTorrent(ih)
	}
	return len(toStop)
}

func (e *RaceEngine) markUserPaused(infoHash string, paused bool) error {
	e.mu.Lock()
	info, ok := e.torrents[infoHash]
	if !ok {
		e.mu.Unlock()
		return errTorrentNotFound
	}
	info.UserPaused = paused
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.UserPaused = paused
	}
	e.cachedStatsMu.Unlock()
	return nil
}

func (e *RaceEngine) syncPausedStats(hashes []string) {
	if len(hashes) == 0 {
		return
	}
	e.cachedStatsMu.Lock()
	for _, ih := range hashes {
		if st, ok := e.cachedStats[ih]; ok {
			st.UserPaused = true
		}
	}
	e.cachedStatsMu.Unlock()
}

// autoStart is the start path every automatic scheduler must use — see the hoard
// version above.
func (e *RaceEngine) autoStart(infoHash string) error {
	if e.IsUserPaused(infoHash) {
		return nil
	}
	if e.client == nil {
		return errEngineNotReady
	}
	return e.client.StartTorrent(infoHash)
}

var (
	errTorrentNotFound = errors.New("torrent not found")
	errEngineNotReady  = errors.New("engine client not ready")
)
