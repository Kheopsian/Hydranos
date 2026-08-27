package api

import (
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"strings"

	"github.com/Kheopsian/hydra/internal/btmeta"
	"github.com/gin-gonic/gin"
)

// Editing a torrent's trackers touches two places that must not disagree: the
// running engine, which decides what gets announced next cycle, and the stored
// .torrent, which decides what comes back after a restart. Writing one without
// the other gives an edit that works until the daemon restarts, or one that
// only appears after it — both look like the feature is broken at random.
//
// So every edit here is: read the live list, transform it in memory, hand the
// whole list to the engine, and only if that succeeded rewrite the blob. The
// engine is the one that can refuse; the store cannot, so it goes second.

// trackerEditor is the slice of an engine this file needs. Both engines satisfy
// it, and naming it here keeps the handler from caring which one it got.
type trackerEditor interface {
	HasTorrent(infoHash string) bool
	GetTrackerTiers(infoHash string) ([][]string, error)
	SetTrackerTiers(infoHash string, tiers [][]string) ([][]string, error)
	TorrentFilePath(infoHash string) (string, bool)
}

func (s *Server) engineHolding(infoHash string) (trackerEditor, string, bool) {
	if s.raceEngine != nil && s.raceEngine.HasTorrent(infoHash) {
		return s.raceEngine, "race", true
	}
	if s.hoardEngine != nil && s.hoardEngine.HasTorrent(infoHash) {
		return s.hoardEngine, "hoard", true
	}
	return nil, "", false
}

// normalizeTrackerURL trims and validates one URL. Trackers are compared as
// exact strings everywhere else in Hydra (counters, overrides, the Trackers
// tab), so the only normalisation done here is whitespace: silently rewriting
// a URL would make the entry the operator sees differ from the one we announce
// to, and from the key their per-tracker passkey is filed under.
func normalizeTrackerURL(raw string) (string, error) {
	u := strings.TrimSpace(raw)
	if u == "" {
		return "", errors.New("empty tracker URL")
	}
	parsed, err := url.Parse(u)
	if err != nil {
		return "", fmt.Errorf("%q is not a URL: %w", u, err)
	}
	switch parsed.Scheme {
	case "http", "https", "udp":
	default:
		return "", fmt.Errorf("%q: a tracker URL has to start with http://, https:// or udp://", u)
	}
	if parsed.Host == "" {
		return "", fmt.Errorf("%q has no host", u)
	}
	return u, nil
}

// applyTrackerOp is the whole edit vocabulary, kept as pure list arithmetic so
// it can be tested without an engine or a database.
//
// Returns the new tiers and whether anything actually changed — an edit that
// changes nothing must not cost a blob rewrite across a bulk selection.
func applyTrackerOp(tiers [][]string, op string, urls []string, from, to string) ([][]string, bool, error) {
	has := func(list [][]string, want string) bool {
		for _, tier := range list {
			for _, u := range tier {
				if u == want {
					return true
				}
			}
		}
		return false
	}
	clone := func(list [][]string) [][]string {
		out := make([][]string, 0, len(list))
		for _, tier := range list {
			cp := append([]string(nil), tier...)
			out = append(out, cp)
		}
		return out
	}
	// Drop tiers that ended up empty: the announce loop walks tiers in order and
	// an empty one is a level that can never answer.
	compact := func(list [][]string) [][]string {
		out := make([][]string, 0, len(list))
		for _, tier := range list {
			t := make([]string, 0, len(tier))
			for _, u := range tier {
				if u != "" {
					t = append(t, u)
				}
			}
			if len(t) > 0 {
				out = append(out, t)
			}
		}
		return out
	}

	switch op {
	case "add":
		out := clone(tiers)
		added := false
		for _, raw := range urls {
			u, err := normalizeTrackerURL(raw)
			if err != nil {
				return nil, false, err
			}
			// Adding one we already have is a no-op, not a duplicate: a torrent
			// announcing twice to the same URL doubles its own load and looks
			// like two peers to the tracker.
			if has(out, u) {
				continue
			}
			out = append(out, []string{u})
			added = true
		}
		return compact(out), added, nil

	case "remove":
		want := map[string]bool{}
		for _, raw := range urls {
			if u := strings.TrimSpace(raw); u != "" {
				want[u] = true
			}
		}
		if len(want) == 0 {
			return nil, false, errors.New("remove needs at least one URL")
		}
		out := make([][]string, 0, len(tiers))
		removed := false
		for _, tier := range tiers {
			keep := make([]string, 0, len(tier))
			for _, u := range tier {
				if want[u] {
					removed = true
					continue
				}
				keep = append(keep, u)
			}
			out = append(out, keep)
		}
		return compact(out), removed, nil

	case "replace":
		src := strings.TrimSpace(from)
		if src == "" {
			return nil, false, errors.New("replace needs the URL to change")
		}
		dst, err := normalizeTrackerURL(to)
		if err != nil {
			return nil, false, err
		}
		if !has(tiers, src) {
			return nil, false, fmt.Errorf("this torrent does not announce to %q", src)
		}
		out := clone(tiers)
		changed := false
		for i, tier := range out {
			for j, u := range tier {
				if u == src {
					out[i][j] = dst
					changed = changed || src != dst
				}
			}
		}
		return compact(out), changed, nil

	case "set":
		// urls carries a flat list here: one tier per URL, which is what the
		// detail view produces. Callers wanting real tiers use the tiers field.
		out := make([][]string, 0, len(urls))
		for _, raw := range urls {
			u, err := normalizeTrackerURL(raw)
			if err != nil {
				return nil, false, err
			}
			out = append(out, []string{u})
		}
		return compact(out), true, nil

	default:
		return nil, false, fmt.Errorf("unknown operation %q: use add, remove, replace or set", op)
	}
}

// checkPersistable fails BEFORE the engine is touched when the edit could not
// be saved anyway. Without it the engine takes the change, the store write
// fails, and the caller gets an error describing a change that did happen: the
// two ends disagree and nothing says which one is right.
func checkPersistable(infoHash string) error {
	st := durable()
	if st == nil {
		return errors.New("no durable store: an edit could not be saved, so it is refused rather than applied to the running agent only")
	}
	rec, ok, err := st.Get(infoHash)
	if err != nil {
		return err
	}
	if !ok || rec == nil || len(rec.Torrent) == 0 {
		return errors.New("this torrent has no stored .torrent yet, so the edit could not be saved. A torrent added moments ago is written to the store on the next state sync; try again shortly")
	}
	return nil
}

// persistTrackers rewrites the .torrent in BOTH places that outlive the
// process, because they are read by different things:
//
//   - the store row, which is the catalogue and what the Go side reloads;
//   - the file under uploads/, which is what the ENGINE re-parses at startup.
//     Its resume record carries no tracker URLs, and materializeBlob only
//     writes that file when it is missing, so leaving it stale means the edit
//     is live, correctly stored, and silently undone by the next restart.
//
// Writing one without the other is the bug this function exists to not have.
func persistTrackers(infoHash string, tiers [][]string, eng trackerEditor) error {
	st := durable()
	if st == nil {
		return errors.New("no durable store: the change is live but would be lost on restart")
	}
	rec, ok, err := st.Get(infoHash)
	if err != nil {
		return err
	}
	if !ok || rec == nil || len(rec.Torrent) == 0 {
		return errors.New("no stored .torrent for this hash: the change is live but would be lost on restart")
	}
	rewritten, err := btmeta.SetAnnounce(rec.Torrent, tiers)
	if err != nil {
		return err
	}
	// Belt and braces: the package guarantees the info dict is copied through,
	// and here we refuse to write anything that failed that guarantee. Getting
	// this wrong turns a torrent into a different torrent.
	before, err1 := btmeta.InfoSpan(rec.Torrent)
	after, err2 := btmeta.InfoSpan(rewritten)
	if err1 != nil || err2 != nil || len(before) != len(after) || string(before) != string(after) {
		return errors.New("refusing to write: the rewrite would have changed the info dict")
	}
	if err := st.UpdateTorrentBlob(infoHash, rewritten); err != nil {
		return err
	}

	// The engine's own copy. A missing path is not an error: a torrent the
	// engine holds without a file on disk has nothing stale to correct.
	path, ok := eng.TorrentFilePath(infoHash)
	if !ok {
		return nil
	}
	if err := writeFileAtomic(path, rewritten); err != nil {
		// Put the store back so the two records cannot disagree.
		if rbErr := st.UpdateTorrentBlob(infoHash, rec.Torrent); rbErr != nil {
			return fmt.Errorf("could not rewrite %s (%v) and could not restore the store row either (%v)", path, err, rbErr)
		}
		return fmt.Errorf("could not rewrite %s, so nothing was saved: %w", path, err)
	}
	return nil
}

// writeFileAtomic replaces a file in one step. A half-written .torrent is a
// torrent the engine refuses to load at its next start, which would turn a
// tracker edit into a lost torrent.
func writeFileAtomic(path string, data []byte) error {
	tmp := path + ".tmp-trackers"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	if err := os.Rename(tmp, path); err != nil {
		os.Remove(tmp)
		return err
	}
	return nil
}

type trackerEditRequest struct {
	Op   string   `json:"op"`
	URLs []string `json:"urls"`
	From string   `json:"from"`
	To   string   `json:"to"`
	// Tiers carries an explicit tier structure for op=set. The editor sends
	// this: a flat list would silently flatten every fallback URL a torrent
	// has into its own tier, changing the order trackers are tried in.
	Tiers  [][]string `json:"tiers"`
	Hashes []string   `json:"hashes"`
}

// tiersFromRequest validates an explicit tier list. Empty tiers are dropped;
// an entirely empty result is allowed and means "announce to nobody".
func tiersFromRequest(in [][]string) ([][]string, error) {
	out := make([][]string, 0, len(in))
	for _, tier := range in {
		clean := make([]string, 0, len(tier))
		for _, raw := range tier {
			u, err := normalizeTrackerURL(raw)
			if err != nil {
				return nil, err
			}
			clean = append(clean, u)
		}
		if len(clean) > 0 {
			out = append(out, clean)
		}
	}
	return out, nil
}

// handleGetTorrentTrackers lists the tracker tiers a torrent announces to.
func (s *Server) handleGetTorrentTrackers(c *gin.Context) {
	infoHash := strings.ToLower(c.Param("info_hash"))
	eng, which, ok := s.engineHolding(infoHash)
	if !ok {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}
	tiers, err := eng.GetTrackerTiers(infoHash)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"engine": which, "trackers": tiers})
}

// handleEditTorrentTrackers applies one operation to one torrent.
func (s *Server) handleEditTorrentTrackers(c *gin.Context) {
	infoHash := strings.ToLower(c.Param("info_hash"))
	var req trackerEditRequest
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	tiers, changed, err := s.editTrackersOne(infoHash, req)
	if err != nil {
		status := http.StatusBadRequest
		if errors.Is(err, errNoSuchTorrent) {
			status = http.StatusNotFound
		}
		c.JSON(status, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"trackers": tiers, "changed": changed})
}

var errNoSuchTorrent = errors.New("torrent not found")

// sameTiers reports whether two lists say the same thing. Saving an editor that
// was opened and not modified must not rewrite a .torrent.
func sameTiers(a, b [][]string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if len(a[i]) != len(b[i]) {
			return false
		}
		for j := range a[i] {
			if a[i][j] != b[i][j] {
				return false
			}
		}
	}
	return true
}

// editTrackersOne is the single path every tracker edit goes through, whatever
// called it: this route, the legacy add-tracker route, or the qBittorrent shim.
func (s *Server) editTrackersOne(infoHash string, req trackerEditRequest) ([][]string, bool, error) {
	// A torrent on an agent has no local engine holding it, so this used to
	// report "no such torrent" for something plainly in the list. The agent's
	// data-plane already speaks get/set trackers, so the same edit runs there.
	if ra, engineID, ok := s.findRemoteOwner(infoHash); ok && ra != nil {
		cl, _ := ra.resolveEngine(engineID)
		if cl == nil {
			return nil, false, fmt.Errorf("agent %s is not reachable", ra.name)
		}
		infos, err := cl.GetTrackers(infoHash)
		if err != nil {
			return nil, false, err
		}
		// The agent reports a flat list with a tier number each; the edit
		// helpers work on tiers, so regroup before touching anything.
		var current [][]string
		for _, ti := range infos {
			for len(current) <= ti.Tier {
				current = append(current, nil)
			}
			current[ti.Tier] = append(current[ti.Tier], ti.URL)
		}
		var next [][]string
		var changed bool
		if req.Op == "set" && len(req.Tiers) > 0 {
			if next, err = tiersFromRequest(req.Tiers); err != nil {
				return nil, false, err
			}
			changed = !sameTiers(current, next)
		} else if next, changed, err = applyTrackerOp(current, req.Op, req.URLs, req.From, req.To); err != nil {
			return nil, false, err
		}
		if !changed {
			return current, false, nil
		}
		applied, err := cl.SetTrackers(infoHash, next)
		if err != nil {
			return nil, false, err
		}
		return applied, true, nil
	}

	eng, _, ok := s.engineHolding(infoHash)
	if !ok {
		return nil, false, errNoSuchTorrent
	}
	current, err := eng.GetTrackerTiers(infoHash)
	if err != nil {
		return nil, false, err
	}
	var next [][]string
	var changed bool
	if req.Op == "set" && len(req.Tiers) > 0 {
		next, err = tiersFromRequest(req.Tiers)
		if err != nil {
			return nil, false, err
		}
		changed = !sameTiers(current, next)
	} else {
		next, changed, err = applyTrackerOp(current, req.Op, req.URLs, req.From, req.To)
		if err != nil {
			return nil, false, err
		}
	}
	if !changed {
		return current, false, nil
	}
	// Refuse early when saving is impossible: an edit that only exists in
	// memory is worse than no edit, because nothing on screen says so.
	if err := checkPersistable(infoHash); err != nil {
		return nil, false, err
	}
	applied, err := eng.SetTrackerTiers(infoHash, next)
	if err != nil {
		return nil, false, err
	}
	// Persist what the engine kept, not what we asked for: if it dropped
	// something, the file has to say the same thing the daemon is doing.
	if err := persistTrackers(infoHash, applied, eng); err != nil {
		// The pre-flight passed and the write still failed (disk full, row
		// deleted underneath us). Put the engine back where it was so the two
		// ends never disagree, and say plainly which of the two happened.
		if _, rbErr := eng.SetTrackerTiers(infoHash, current); rbErr != nil {
			return applied, true, fmt.Errorf(
				"could not save the change (%v) AND could not undo it on the running agent (%v): "+
					"the agent is announcing the new list but the stored .torrent still holds the old one", err, rbErr)
		}
		return current, false, fmt.Errorf("could not save the change, so it was undone: %w", err)
	}
	return applied, true, nil
}
