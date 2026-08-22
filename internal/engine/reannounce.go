package engine

import (
	"log/slog"
	"time"
)

// ReannounceNow forces one immediate announce for a torrent held by this
// engine, to every tracker it carries, and reports whether anything went out.
//
// The API has exposed a reannounce route for a long time, but nothing behind
// it: the adapter answered true and did nothing at all, so the endpoint
// reported success on an announce that never happened. That matters most right
// after a tracker is added for cross-seeding, when the whole point is not to
// wait out a re-announce interval before the new swarm hears from us.
func (e *RaceEngine) ReannounceNow(infoHash string) bool {
	if e == nil || e.client == nil {
		return false
	}
	e.mu.RLock()
	_, tracked := e.torrents[infoHash]
	e.mu.RUnlock()
	if !tracked {
		return false
	}

	st, err := e.client.GetStatus(infoHash)
	if err != nil {
		return false
	}

	var urls []string
	if trks, terr := e.client.GetTrackers(infoHash); terr == nil {
		for _, t := range trks {
			if isSupportedTrackerScheme(t.URL) {
				urls = append(urls, t.URL)
			}
		}
	}
	if len(urls) == 0 {
		return false
	}

	left := st.TotalSize - st.TotalDone
	if left < 0 {
		left = 0
	}
	// A finished torrent announces left=0, which is what makes the tracker
	// record it as a seeder rather than a leecher.
	if st.TotalSize > 0 && st.TotalDone >= st.TotalSize {
		left = 0
	}

	announcer := e.announcer()
	now := time.Now()
	sent := 0
	for _, u := range urls {
		host := overrideHost(u)
		// A manual reannounce is a deliberate act, so it is worth spending on
		// a host the breaker has parked -- but a failure still counts, or one
		// impatient caller could keep a dead tracker permanently in service.
		result, aerr := announcer.announce(u, infoHash, st.TotalUpload, st.TotalDownload, left, "")
		if aerr != nil {
			e.breaker.record(host, false, now)
			continue
		}
		e.breaker.record(host, true, now)
		if result == nil || result.FailureReason != "" {
			continue
		}
		sent++
		if len(result.Peers) > 0 {
			peers := make([]struct {
				IP   string
				Port int
			}, len(result.Peers))
			for i, p := range result.Peers {
				peers[i] = struct {
					IP   string
					Port int
				}{p.IP, p.Port}
			}
			e.client.AddPeers(infoHash, peers)
		}
	}

	if sent > 0 {
		e.mu.Lock()
		e.lastAnnounceTime[infoHash] = now
		e.mu.Unlock()
		slog.Info("race: manual reannounce", "info_hash", infoHash[:minStr(len(infoHash), 8)],
			"trackers", len(urls), "ok", sent)
	}
	return sent > 0
}

// ReannounceNow fires one immediate seeder announce for a hoard torrent,
// reusing the announcer already wired for the in-place operations that drop us
// out of a swarm. Reports whether it had somewhere to send it.
func (e *HoardEngine) ReannounceNow(infoHash string) bool {
	if e == nil || e.reAnnounce == nil || !e.HasTorrent(infoHash) {
		return false
	}
	var totalSize int64
	if e.client != nil {
		if st, err := e.client.GetStatus(infoHash); err == nil {
			totalSize = st.TotalSize
		}
	}
	// Fire and forget, like every other hoard announce: a tracker that is slow
	// must not hold an API request open.
	go e.reAnnounce(infoHash, totalSize)
	return true
}
