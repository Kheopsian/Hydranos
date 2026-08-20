package api

import (
	"encoding/json"
	"errors"
	"net/http"
	"strconv"

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
	src, dst, _, _, err := s.resolveMovePaths(hash, target)
	if err != nil {
		c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
		return
	}
	if src == dst {
		c.JSON(http.StatusOK, gin.H{"move_needed": false})
		return
	}
	plan, err := move.Inspect(src, dst)
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
	src, dst, engineSavePath, _, err := s.resolveMovePaths(hash, category)
	if err != nil {
		c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
		return false
	}
	if src == dst {
		return true // nothing to move; caller carries on with the label change
	}
	plan, err := move.Inspect(src, dst)
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
		EngineSavePath:         engineSavePath,
		Category:               category,
		AllowBreakingHardlinks: allowBreakingHardlinks,
		BytesPerSecond:         s.config.Daemon.MoveBytesPerSecond(),
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
