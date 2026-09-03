package api

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/jobs"
	"github.com/Kheopsian/hydra/internal/move"
	"github.com/Kheopsian/hydra/internal/store"
)

// Background work, over HTTP.
//
// A move takes minutes to hours, so the API that starts one returns a job id
// immediately and everything else is asking about that job. The same endpoints
// will serve whatever a rules engine schedules later -- they are about jobs,
// not about moves.

// jobView is the wire shape. Params are inlined as raw JSON so a caller can
// read a move's source and target without the API having to know about moves.
type jobView struct {
	ID            string          `json:"id"`
	Type          string          `json:"type"`
	State         string          `json:"state"`
	InfoHash      string          `json:"info_hash,omitempty"`
	Params        json.RawMessage `json:"params,omitempty"`
	ProgressBytes int64           `json:"progress_bytes"`
	TotalBytes    int64           `json:"total_bytes"`
	Percent       float64         `json:"percent"`
	Error         string          `json:"error,omitempty"`
	CreatedAt     int64           `json:"created_at"`
	UpdatedAt     int64           `json:"updated_at"`
}

func toJobView(j *store.Job) jobView {
	v := jobView{
		ID:            j.ID,
		Type:          j.Type,
		State:         string(j.State),
		InfoHash:      j.InfoHash,
		ProgressBytes: j.ProgressBytes,
		TotalBytes:    j.TotalBytes,
		Error:         j.Error,
		CreatedAt:     j.CreatedAt.Unix(),
		UpdatedAt:     j.UpdatedAt.Unix(),
	}
	if j.Params != "" && json.Valid([]byte(j.Params)) {
		v.Params = json.RawMessage(j.Params)
	}
	if j.TotalBytes > 0 {
		v.Percent = float64(j.ProgressBytes) / float64(j.TotalBytes) * 100
	}
	return v
}

func (s *Server) handleJobsList(c *gin.Context) {
	if s.jobs == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "background jobs are not running on this node"})
		return
	}
	limit := 100
	if raw := c.Query("limit"); raw != "" {
		if n, err := strconv.Atoi(raw); err == nil && n >= 0 {
			limit = n
		}
	}
	list, err := s.jobs.List(limit)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	out := make([]jobView, 0, len(list))
	for _, j := range list {
		out = append(out, toJobView(j))
	}
	c.JSON(http.StatusOK, out)
}

func (s *Server) handleJobGet(c *gin.Context) {
	if s.jobs == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "background jobs are not running on this node"})
		return
	}
	j, ok, err := s.jobs.Get(c.Param("id"))
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if !ok {
		c.JSON(http.StatusNotFound, gin.H{"error": "no such job"})
		return
	}
	c.JSON(http.StatusOK, toJobView(j))
}

// handleJobCancel stops a running job. What a cancelled job leaves behind is
// the runner's business; for a move it is the source untouched and the partial
// copy cleaned up.
func (s *Server) handleJobCancel(c *gin.Context) {
	if s.jobs == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "background jobs are not running on this node"})
		return
	}
	if err := s.jobs.Cancel(c.Param("id")); err != nil {
		c.JSON(http.StatusConflict, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

// handleMovePreview reports what moving a torrent's payload to a category
// would involve, without touching anything.
//
// This is what the UI calls before showing the modal: it is the difference
// between "this will break 3 hardlinks, continue?" and finding out afterwards.
func (s *Server) handleMovePreview(c *gin.Context) {
	hash := c.Param("info_hash")
	target := c.Query("category")
	if target == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "category required"})
		return
	}
	mp, err := s.resolveMovePaths(hash, target)
	if err != nil {
		c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
		return
	}
	if mp.Source == mp.Target {
		c.JSON(http.StatusOK, gin.H{"move_needed": false})
		return
	}
	plan, err := inspectFor(mp, hash)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	body := gin.H{
		"move_needed":            true,
		"source":                 plan.Source,
		"target":                 plan.Target,
		"total_bytes":            plan.TotalBytes,
		"file_count":             len(plan.Files),
		"same_filesystem":        plan.SameFS,
		"free_bytes_on_target":   plan.FreeBytes,
		"hardlinked_files":       plan.HardlinkedFiles,
		"hardlinked_bytes":       plan.HardlinkedBytes,
		"hardlink_examples":      plan.HardlinkExamples,
		"needs_hardlink_consent": !plan.SameFS && plan.HardlinkedFiles > 0,
		// Loose says the payload has no folder of its own and will land
		// directly in the category directory, keeping the layout it has.
		"loose": plan.Loose,
	}
	// Report the blocking problems the same way the submit path will, so the
	// UI never shows a green preview for something that will be refused.
	if err := plan.Check(true); err != nil {
		body["blocked"] = err.Error()
	}
	c.JSON(http.StatusOK, body)
}

// submitMoveJob queues the payload relocation behind a category change.
//
// Returns 409 with a machine-readable reason when the operator has to decide
// something -- hardlinks above all -- rather than guessing on their behalf.
func (s *Server) submitMoveJob(c *gin.Context, hash, category string, allowBreakingHardlinks bool) bool {
	mp, err := s.resolveMovePaths(hash, category)
	if err != nil {
		c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
		return false
	}
	if mp.Source == mp.Target {
		return true // nothing to move; caller carries on with the label change
	}
	plan, err := inspectFor(mp, hash)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return false
	}
	if err := plan.Check(allowBreakingHardlinks); err != nil {
		status := http.StatusConflict
		body := gin.H{"error": err.Error()}
		switch {
		case errors.Is(err, move.ErrWouldBreakHardlinks):
			body["reason"] = "hardlinks"
			body["hardlinked_files"] = plan.HardlinkedFiles
			body["hardlinked_bytes"] = plan.HardlinkedBytes
			body["hardlink_examples"] = plan.HardlinkExamples
			body["retry_with"] = "allow_breaking_hardlinks"
		case errors.Is(err, move.ErrTargetFileExists):
			// A loose move refused per file: the names it needs are
			// taken by torrents already in the target category.
			body["reason"] = "target_file_exists"
			body["target"] = plan.Target
			body["collisions"] = plan.Collisions
		case errors.Is(err, move.ErrTargetExists):
			body["reason"] = "target_exists"
			body["target"] = plan.Target
		case errors.Is(err, move.ErrNotEnoughSpace):
			body["reason"] = "no_space"
			body["needed_bytes"] = plan.TotalBytes
			body["free_bytes"] = plan.FreeBytes
			status = http.StatusInsufficientStorage
		}
		c.JSON(status, body)
		return false
	}

	j, err := s.jobs.Submit(store.JobTypeMoveData, hash, jobs.MoveParams{
		Source:                 plan.Source,
		Target:                 plan.Target,
		EngineSavePath:         mp.EngineSavePath,
		Category:               category,
		Name:                   mp.Name,
		AllowBreakingHardlinks: allowBreakingHardlinks,
		BytesPerSecond:         s.config.Daemon.MoveBytesPerSecond(),
		Loose:                  plan.Loose,
		Files:                  plan.Files,
	})
	if err != nil {
		c.JSON(http.StatusConflict, gin.H{"error": err.Error()})
		return false
	}
	c.JSON(http.StatusAccepted, gin.H{
		"status":      "moving",
		"job_id":      j.ID,
		"info_hash":   hash,
		"category":    category,
		"source":      plan.Source,
		"target":      plan.Target,
		"total_bytes": plan.TotalBytes,
		"same_fs":     plan.SameFS,
	})
	return false
}

// CategorySavePathFor is where a category puts payloads on a given agent.
//
// For a REMOTE agent a missing mapping is an error, never a fallback. The
// category's flat SavePath is a path on THIS host -- handing "/data/movies" to
// a Windows agent would land the payload nowhere and report success. The local
// node keeps the ordinary fallback, where that path is the right answer.
func (s *Server) CategorySavePathFor(agent, name string) (string, error) {
	cat := s.categoryByName(name)
	if cat == nil {
		return "", fmt.Errorf("no category named %q", name)
	}
	if agent == "" || isLocalAgentName(agent) {
		if cat.SavePath == "" {
			return "", fmt.Errorf("category %q has no save path", name)
		}
		return cat.SavePath, nil
	}
	if pth := cat.Agents[agent]; pth != "" {
		return pth, nil
	}
	return "", fmt.Errorf("category %q defines no save path for agent %q: set one on the category before moving there", name, agent)
}

// handleMoveRemote submits a cross-node payload move or duplicate.
//
// The destination path is NOT a parameter: it is whatever the category defines
// for the target agent. A free-text path typed for one host is meaningless on
// another, and re-categorising is its own action, before or after.
func (s *Server) handleMoveRemote(c *gin.Context) {
	if s.jobs == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "background jobs are not running on this node"})
		return
	}
	var req struct {
		InfoHash       string `json:"info_hash"`
		SourceAgent    string `json:"source_agent"`
		TargetAgent    string `json:"target_agent"`
		Engine         string `json:"engine"`
		Category       string `json:"category"`
		Mode           string `json:"mode"` // "move" | "duplicate"
		KeepSourceData bool   `json:"keep_source_data"`
		Name           string `json:"name"`
		BytesPerSecond int64  `json:"bytes_per_second"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.Mode == "" {
		req.Mode = "move"
	}
	switch {
	case req.InfoHash == "":
		c.JSON(http.StatusBadRequest, gin.H{"error": "info_hash is required"})
		return
	case req.TargetAgent == "":
		c.JSON(http.StatusBadRequest, gin.H{"error": "target_agent is required"})
		return
	case req.SourceAgent == req.TargetAgent:
		c.JSON(http.StatusBadRequest, gin.H{"error": "source and target are the same node"})
		return
	case req.Mode != "move" && req.Mode != "duplicate":
		c.JSON(http.StatusBadRequest, gin.H{"error": `mode must be "move" or "duplicate"`})
		return
	}
	// One engine id per END. A single field was true while a move meant "the
	// same engine on another machine"; handing a torrent from local-hoard to
	// local-vpn7 asks for a different engine on each side, and one field sent
	// whichever was resolved first to both.
	sourceEngine := s.engineOfAgent(req.SourceAgent)
	targetEngine := s.engineOfAgent(req.TargetAgent)
	if req.Engine == "" {
		req.Engine = sourceEngine
	}
	if req.Engine == "" {
		req.Engine = "hoard"
	}
	if sourceEngine == "" {
		sourceEngine = req.Engine
	}
	if targetEngine == "" {
		targetEngine = req.Engine
	}
	// The destination is the path THIS torrent's category defines on the target
	// agent, so the category is read from the torrent rather than passed in.
	// Re-categorising is its own action; asking for it here would let the two
	// disagree, and the payload would land somewhere the torrent does not say
	// it lives.
	if req.Category == "" {
		req.Category = s.torrentCategory(req.InfoHash)
	}
	if req.Category == "" {
		c.JSON(http.StatusBadRequest, gin.H{
			"error": "this torrent has no category, and the destination path comes from the category: set one first"})
		return
	}
	// Resolve everything the job will need NOW, so a bad category or an
	// unknown agent is a 400 on this request instead of a job that fails a
	// second later somewhere the user is not looking.
	targetPath, err := s.CategorySavePathFor(req.TargetAgent, req.Category)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	// Between two engines of THIS node the payload is already where the target
	// expects it, so the torrent changes hands where it lies. Deciding it here
	// rather than in the runner keeps the refusal in the request: a handoff
	// cannot duplicate, because two engines seeding one set of files are two
	// writers on the same bytes the first time either repairs a piece.
	handoff := false
	if isLocalAgentName(req.SourceAgent) && isLocalAgentName(req.TargetAgent) {
		srcPath, perr := s.CategorySavePathFor(req.SourceAgent, req.Category)
		if perr == nil && srcPath == targetPath {
			handoff = true
		}
	}
	if handoff && req.Mode != "move" {
		c.JSON(http.StatusBadRequest, gin.H{
			"error": "these two agents share a filesystem, so the torrent can be handed over but not duplicated: both copies would be the same files"})
		return
	}
	for label, end := range map[string][2]string{
		"source": {req.SourceAgent, sourceEngine},
		"target": {req.TargetAgent, targetEngine},
	} {
		if end[0] == "" || isLocalAgentName(end[0]) {
			continue
		}
		if _, err := s.RemoteAgentEngineClient(end[0], end[1]); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": label + ": " + err.Error()})
			return
		}
	}

	// A job outlives the torrent it acts on, and a hash is not something anyone
	// can read in a list. Resolve the name here rather than trusting the caller
	// to pass one: the UI has it, but the API is also driven by hand.
	if req.Name == "" {
		req.Name = s.torrentName(req.InfoHash)
	}

	job, err := s.jobs.Submit(store.JobTypeMoveDataRemote, req.InfoHash, jobs.RemoteMoveParams{
		SourceAgent:    req.SourceAgent,
		TargetAgent:    req.TargetAgent,
		Engine:         req.Engine,
		SourceEngine:   sourceEngine,
		TargetEngine:   targetEngine,
		Category:       req.Category,
		ReleaseSource:  req.Mode == "move",
		KeepSourceData: req.KeepSourceData,
		Name:           req.Name,
		BytesPerSecond: req.BytesPerSecond,
		Handoff:        handoff,
	})
	if err != nil {
		c.JSON(http.StatusConflict, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusAccepted, gin.H{"status": "ok", "job_id": job.ID, "mode": req.Mode, "handoff": handoff})
}

// torrentCategory reads a torrent's current category from whichever engine
// holds it. Empty when nothing knows it.
func (s *Server) torrentCategory(infoHash string) string {
	pick := func(m map[string]interface{}) string {
		if m == nil {
			return ""
		}
		if v, ok := m["category"].(string); ok {
			return v
		}
		return ""
	}
	if s.hoardEngine != nil {
		if cat := pick(s.hoardEngine.GetTorrentDetail(infoHash)); cat != "" {
			return cat
		}
	}
	if s.raceEngine != nil {
		if cat := pick(s.raceEngine.GetTorrentStatus(infoHash)); cat != "" {
			return cat
		}
	}
	// A torrent that lives on an agent has no local engine to ask, and the
	// local lookup returning "" reads as "no category" -- which refused a move
	// off an agent for a torrent that plainly had one.
	if ra, _, ok := s.findRemoteOwner(infoHash); ok && ra != nil {
		for _, e := range ra.engines {
			if e.client == nil {
				continue
			}
			cats, err := e.client.TorrentCategories(e.id)
			if err != nil {
				continue
			}
			if cat := cats[infoHash]; cat != "" {
				return cat
			}
		}
	}
	return ""
}

// torrentName reads a torrent's display name from whichever node holds it.
func (s *Server) torrentName(infoHash string) string {
	pick := func(m map[string]interface{}) string {
		if m == nil {
			return ""
		}
		if v, ok := m["name"].(string); ok {
			return v
		}
		return ""
	}
	if s.hoardEngine != nil {
		if n := pick(s.hoardEngine.GetTorrentDetail(infoHash)); n != "" {
			return n
		}
	}
	if s.raceEngine != nil {
		if n := pick(s.raceEngine.GetTorrentStatus(infoHash)); n != "" {
			return n
		}
	}
	if ra, _, ok := s.findRemoteOwner(infoHash); ok && ra != nil {
		for _, e := range ra.engines {
			if e.client == nil {
				continue
			}
			lst, err := e.client.ListTorrentsTimeout(4 * time.Second)
			if err != nil || lst == nil {
				continue
			}
			for _, t := range lst.Torrents {
				if strings.EqualFold(t.InfoHash, infoHash) && t.Name != "" {
					return t.Name
				}
			}
		}
	}
	return ""
}

// engineOfAgent names the engine an agent hosts, when it hosts exactly one --
// which is every agent since one agent became one engine. Empty for the bare
// "local" alias, which pins no engine, and for an agent hosting several.
//
// The ID, not the role. Both reach the client, which matches on id first and
// falls back to role -- but the routed calls on the far side resolve by ID
// alone, so a role got as far as the agent and was refused there: "engine
// \"hoard\" not wired". That is how a handed-over torrent arrived with no
// category, which then made it unmovable, since the destination path of a move
// comes from the category.
func (s *Server) engineOfAgent(name string) string {
	if name == "" || name == LocalAgentName {
		return ""
	}
	ra := s.remoteAgentByName(name)
	if ra == nil {
		return ""
	}
	s.agentsMu.RLock()
	defer s.agentsMu.RUnlock()
	if len(ra.engines) != 1 {
		return ""
	}
	return ra.engines[0].id
}
