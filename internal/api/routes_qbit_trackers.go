package api

import (
	"net/http"
	"strings"

	"github.com/gin-gonic/gin"
)

// qBittorrent tracker endpoints. cross-seed and the *arr stack drive
// cross-seeding through these three calls; the shim answered 404 for all of
// them, so a client could add a torrent but never point it at a second
// tracker, and the failure looked like the tool being broken rather than a
// missing route.
//
// The work itself is the native editor (editTrackersOne), which already
// handles a torrent held locally or on an agent, normalises URLs and persists
// the change to the .torrent.

// splitTrackerURLs accepts both separators qBittorrent uses: addTrackers sends
// newline-separated URLs, removeTrackers sends them pipe-separated. Taking
// either on both routes costs nothing and spares a caller a confusing no-op.
func splitTrackerURLs(raw string) []string {
	fields := strings.FieldsFunc(raw, func(r rune) bool {
		return r == '\n' || r == '\r' || r == '|'
	})
	out := make([]string, 0, len(fields))
	for _, f := range fields {
		if f = strings.TrimSpace(f); f != "" {
			out = append(out, f)
		}
	}
	return out
}

// qbitTorrentsAddTrackers implements POST /api/v2/torrents/addTrackers.
func (s *Server) qbitTorrentsAddTrackers(c *gin.Context) {
	s.qbitEditTrackers(c, "add")
}

// qbitTorrentsRemoveTrackers implements POST /api/v2/torrents/removeTrackers.
func (s *Server) qbitTorrentsRemoveTrackers(c *gin.Context) {
	s.qbitEditTrackers(c, "remove")
}

func (s *Server) qbitEditTrackers(c *gin.Context, op string) {
	hash := strings.ToLower(strings.TrimSpace(c.PostForm("hash")))
	urls := splitTrackerURLs(c.PostForm("urls"))
	if hash == "" || len(urls) == 0 {
		// qBittorrent answers 400 on a malformed call, and cross-seed reads
		// the status: reporting 200 here would make a dropped tracker look
		// applied.
		c.String(http.StatusBadRequest, "")
		return
	}

	if _, _, err := s.editTrackersOne(hash, trackerEditRequest{Op: op, URLs: urls}); err != nil {
		if strings.Contains(err.Error(), "not found") {
			c.String(http.StatusNotFound, "")
			return
		}
		c.String(http.StatusBadRequest, "")
		return
	}

	// A freshly added tracker has no announce scheduled until the keepalive
	// comes round, which on a seeding torrent is up to its re-announce
	// interval away. Announce now so the new swarm sees us immediately.
	if op == "add" {
		s.reannounceOne(hash)
	}
	c.String(http.StatusOK, "")
}

// qbitTorrentsReannounce implements POST /api/v2/torrents/reannounce.
func (s *Server) qbitTorrentsReannounce(c *gin.Context) {
	for _, hash := range parseHashes(c.PostForm("hashes")) {
		hash = strings.ToLower(hash)
		if hash == "" || hash == "all" {
			continue
		}
		s.reannounceOne(hash)
	}
	c.String(http.StatusOK, "")
}

// reannounceOne forces one torrent to announce now, wherever it is held.
// Reports whether anything took the request.
func (s *Server) reannounceOne(infoHash string) bool {
	if s.raceEngine != nil && s.raceEngine.HasTorrent(infoHash) {
		if s.raceEngine.ReannnounceTorrent(infoHash) {
			return true
		}
	} else if s.hoardEngine != nil && s.hoardEngine.HasTorrent(infoHash) {
		if s.hoardEngine.ReannnounceTorrent(infoHash) {
			return true
		}
	}
	if ra, mode, ok := s.findRemoteOwner(infoHash); ok && ra != nil {
		if cl := ra.anyClient(); cl != nil {
			return cl.ActionRouted(mode, "reannounce", infoHash, false, "", "") == nil
		}
	}
	return false
}
