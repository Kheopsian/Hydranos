// Package jobs runs the work that cannot fit inside a request.
//
// Relocating a torrent's payload across filesystems takes minutes to hours. It
// therefore cannot be the body of an HTTP handler: the caller would hold a
// connection open for the duration, a restart would lose track of what was
// half-done, and nothing else in Hydra could ask what is currently running.
//
// The shape here is deliberately general. A move is the first kind of job, not
// the only one -- the same records and the same runner back anything that has
// to survive a restart, report progress and be cancellable, which is what a
// rules engine firing "graduate this torrent once it has seeded a week" will
// need from the day it exists.
package jobs

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/store"
)

// Runner executes one kind of job. Registering a type is all it takes to make
// it schedulable, resumable and cancellable.
type Runner interface {
	// Type is the value in the job's `type` column.
	Type() string

	// Run performs the job. It should report progress through report and
	// return promptly when ctx is cancelled. Returning nil means done.
	Run(ctx context.Context, j *store.Job, report func(done, total int64)) error

	// Resume decides what happens to a job that a crash left mid-flight.
	// Returning true re-runs it; false fails it with the given reason. Only
	// the job type knows whether half its work on disk can be picked up.
	Resume(j *store.Job) (bool, string)
}

// Store is the persistence the manager needs. Narrowed to an interface so the
// manager is testable without a database file.
type Store interface {
	PutJob(j *store.Job) error
	GetJob(id string) (*store.Job, bool, error)
	ListJobs(limit int) ([]*store.Job, error)
	UnfinishedJobs() ([]*store.Job, error)
	ActiveJobForTorrent(infoHash string) (*store.Job, bool, error)
	SetJobState(id string, st store.JobState, errMsg string) error
	UpdateJobProgress(id string, done int64) error
	PruneJobs(olderThan time.Time) (int64, error)
}

// Manager owns the job table and the goroutines working through it.
type Manager struct {
	st      Store
	runners map[string]Runner

	// base is the lifetime of the daemon, and every job runs under it.
	//
	// It is held here rather than taken per call for a reason found the hard
	// way: passing the caller's context meant passing an HTTP request's
	// context, so the job was cancelled the instant the request that created
	// it returned -- which is the precise opposite of what a job is for. A
	// job outlives its request by definition, so the API for starting one
	// must not offer a way to say otherwise.
	base context.Context

	mu      sync.Mutex
	cancels map[string]context.CancelFunc

	// concurrency bounds simultaneous jobs. Moves are I/O bound on the same
	// disks the torrents are served from, so running many at once makes all
	// of them slower and the seeding worse.
	sem chan struct{}
}

// NewManager builds a manager whose jobs live as long as ctx -- the daemon's
// lifetime, never a request's. concurrency below 1 is treated as 1.
func NewManager(ctx context.Context, st Store, concurrency int) *Manager {
	if concurrency < 1 {
		concurrency = 1
	}
	return &Manager{
		base:    ctx,
		st:      st,
		runners: map[string]Runner{},
		cancels: map[string]context.CancelFunc{},
		sem:     make(chan struct{}, concurrency),
	}
}

// Register adds a runner. Not safe to call once jobs are running.
func (m *Manager) Register(r Runner) { m.runners[r.Type()] = r }

// Submit records a new job and starts it. The job is durable before this
// returns, so a crash a millisecond later still leaves a trace of the intent.
func (m *Manager) Submit(typ, infoHash string, params interface{}) (*store.Job, error) {
	if _, ok := m.runners[typ]; !ok {
		return nil, fmt.Errorf("jobs: no runner registered for %q", typ)
	}
	if infoHash != "" {
		if existing, ok, err := m.st.ActiveJobForTorrent(infoHash); err != nil {
			return nil, err
		} else if ok {
			return nil, fmt.Errorf("jobs: %s already has job %s in progress (%s)",
				infoHash, existing.ID, existing.State)
		}
	}
	blob, err := json.Marshal(params)
	if err != nil {
		return nil, fmt.Errorf("jobs: encoding params: %w", err)
	}
	j := &store.Job{
		ID:        newID(),
		Type:      typ,
		State:     store.JobPending,
		InfoHash:  infoHash,
		Params:    string(blob),
		CreatedAt: time.Now(),
	}
	if err := m.st.PutJob(j); err != nil {
		return nil, err
	}
	m.start(j)
	return j, nil
}

// ResumeAll picks up whatever the last shutdown interrupted. Called once at
// startup, before the API starts accepting new work.
func (m *Manager) ResumeAll() {
	pending, err := m.st.UnfinishedJobs()
	if err != nil {
		slog.Error("jobs: could not read unfinished jobs", "error", err)
		return
	}
	for _, j := range pending {
		r, ok := m.runners[j.Type]
		if !ok {
			_ = m.st.SetJobState(j.ID, store.JobFailed, "no runner for job type "+j.Type)
			continue
		}
		if j.State == store.JobPending {
			m.start(j)
			continue
		}
		resume, reason := r.Resume(j)
		if !resume {
			slog.Warn("jobs: abandoning interrupted job", "id", j.ID, "type", j.Type, "reason", reason)
			_ = m.st.SetJobState(j.ID, store.JobFailed, reason)
			continue
		}
		slog.Info("jobs: resuming interrupted job", "id", j.ID, "type", j.Type)
		m.start(j)
	}
}

// Cancel stops a running job. The runner decides what a cancelled job leaves
// behind; the contract is that it must not be a half-applied change.
func (m *Manager) Cancel(id string) error {
	m.mu.Lock()
	cancel, ok := m.cancels[id]
	m.mu.Unlock()
	if !ok {
		j, found, err := m.st.GetJob(id)
		if err != nil {
			return err
		}
		if !found {
			return fmt.Errorf("jobs: no such job %s", id)
		}
		if j.State.Terminal() {
			return fmt.Errorf("jobs: job %s already finished (%s)", id, j.State)
		}
		// Recorded but not running: mark it so the runner never picks it up.
		return m.st.SetJobState(id, store.JobCancelled, "cancelled before it started")
	}
	cancel()
	return nil
}

// List returns recent jobs, newest first.
func (m *Manager) List(limit int) ([]*store.Job, error) { return m.st.ListJobs(limit) }

// Get returns one job.
func (m *Manager) Get(id string) (*store.Job, bool, error) { return m.st.GetJob(id) }

// Prune drops finished jobs older than age.
func (m *Manager) Prune(age time.Duration) {
	if n, err := m.st.PruneJobs(time.Now().Add(-age)); err == nil && n > 0 {
		slog.Info("jobs: pruned finished jobs", "count", n)
	}
}

func (m *Manager) start(j *store.Job) {
	ctx, cancel := context.WithCancel(m.base)
	m.mu.Lock()
	m.cancels[j.ID] = cancel
	m.mu.Unlock()

	go func() {
		defer func() {
			cancel()
			m.mu.Lock()
			delete(m.cancels, j.ID)
			m.mu.Unlock()
		}()

		// Wait for a slot. A queued job stays `pending` in the table, which
		// is what the UI should show: queued, not stuck.
		select {
		case m.sem <- struct{}{}:
			defer func() { <-m.sem }()
		case <-ctx.Done():
			_ = m.st.SetJobState(j.ID, store.JobCancelled, "cancelled while queued")
			return
		}

		r := m.runners[j.Type]
		if err := m.st.SetJobState(j.ID, store.JobRunning, ""); err != nil {
			slog.Error("jobs: could not mark job running", "id", j.ID, "error", err)
			return
		}

		// Progress writes are throttled: a copy reports every 32 MiB, and at
		// several hundred MB/s that would otherwise be a database write many
		// times a second for hours.
		var lastWrite time.Time
		report := func(done, total int64) {
			if total > 0 && j.TotalBytes != total {
				j.TotalBytes = total
				j.ProgressBytes = done
				_ = m.st.PutJob(j)
				lastWrite = time.Now()
				return
			}
			j.ProgressBytes = done
			if time.Since(lastWrite) > 2*time.Second {
				_ = m.st.UpdateJobProgress(j.ID, done)
				lastWrite = time.Now()
			}
		}

		err := r.Run(ctx, j, report)
		_ = m.st.UpdateJobProgress(j.ID, j.ProgressBytes)
		switch {
		case err == nil:
			_ = m.st.SetJobState(j.ID, store.JobDone, "")
			slog.Info("jobs: finished", "id", j.ID, "type", j.Type)
		case ctx.Err() != nil:
			_ = m.st.SetJobState(j.ID, store.JobCancelled, "cancelled")
			slog.Info("jobs: cancelled", "id", j.ID, "type", j.Type)
		default:
			_ = m.st.SetJobState(j.ID, store.JobFailed, err.Error())
			slog.Error("jobs: failed", "id", j.ID, "type", j.Type, "error", err)
		}
	}()
}

// newID is a sortable, collision-proof-enough identifier: the timestamp makes
// job lists readable in creation order, the counter separates jobs submitted
// in the same nanosecond.
var idSeq struct {
	sync.Mutex
	n uint64
}

func newID() string {
	idSeq.Lock()
	idSeq.n++
	n := idSeq.n
	idSeq.Unlock()
	return fmt.Sprintf("%d-%d", time.Now().UnixNano(), n)
}
