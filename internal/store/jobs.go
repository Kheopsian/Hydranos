package store

import (
	"database/sql"
	"fmt"
	"time"
)

// Long-running work, durably recorded.
//
// Moving a torrent's payload across filesystems takes minutes to hours, so it
// cannot be the body of an HTTP request: the caller would have to hold a
// connection open, a restart would lose track of what was half-done, and
// nothing else in Hydra could ask what is currently running. It is a job.
//
// The table is deliberately generic. A move is the first kind of job, not the
// only one: the same records back anything that has to survive a restart,
// report progress and be cancellable -- which is exactly what a rules engine
// firing "graduate this torrent when it has seeded for a week" needs.

// JobState is where a job is in its life.
type JobState string

const (
	JobPending   JobState = "pending"
	JobRunning   JobState = "running"
	JobVerifying JobState = "verifying"
	JobDone      JobState = "done"
	JobFailed    JobState = "failed"
	JobCancelled JobState = "cancelled"
)

// Terminal reports whether a job will never change state again.
func (s JobState) Terminal() bool {
	return s == JobDone || s == JobFailed || s == JobCancelled
}

// JobTypeMoveData relocates a torrent's payload files.
const JobTypeMoveData = "move_data"

// JobTypeMoveDataRemote relocates a payload to another node, bytes included.
// Separate from JobTypeMoveData because the two fail in different ways and
// resume from different state: a local move resumes from a staging directory,
// a remote one from the destination's bitfield.
const JobTypeMoveDataRemote = "move_data_remote"

// Job is one unit of durable background work.
type Job struct {
	ID       string
	Type     string
	State    JobState
	InfoHash string
	// Params is a JSON document owned by the job type: source and target
	// paths, whether breaking hardlinks was authorised, and so on. Kept
	// opaque here so a new job type needs no schema change.
	Params        string
	ProgressBytes int64
	TotalBytes    int64
	Error         string
	CreatedAt     time.Time
	UpdatedAt     time.Time
}

// PutJob inserts or replaces a job wholesale.
func (s *Store) PutJob(j *Job) error {
	if j.ID == "" {
		return fmt.Errorf("store: job needs an id")
	}
	s.wmux.Lock()
	defer s.wmux.Unlock()
	now := time.Now().Unix()
	created := j.CreatedAt.Unix()
	if j.CreatedAt.IsZero() {
		created = now
	}
	_, err := s.db.Exec(`
        INSERT INTO jobs (id, type, state, info_hash, params, progress_bytes, total_bytes, error, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
            type=excluded.type, state=excluded.state, info_hash=excluded.info_hash,
            params=excluded.params, progress_bytes=excluded.progress_bytes,
            total_bytes=excluded.total_bytes, error=excluded.error,
            updated_at=excluded.updated_at`,
		j.ID, j.Type, string(j.State), j.InfoHash, j.Params,
		j.ProgressBytes, j.TotalBytes, j.Error, created, now)
	return err
}

// UpdateJobProgress records forward movement without rewriting the whole row.
// Called often during a copy, so it touches only the columns that change.
func (s *Store) UpdateJobProgress(id string, done int64) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`UPDATE jobs SET progress_bytes=?, updated_at=? WHERE id=?`,
		done, time.Now().Unix(), id)
	return err
}

// SetJobState moves a job to a new state, recording the reason on failure.
func (s *Store) SetJobState(id string, st JobState, errMsg string) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`UPDATE jobs SET state=?, error=?, updated_at=? WHERE id=?`,
		string(st), errMsg, time.Now().Unix(), id)
	return err
}

// GetJob returns one job; ok is false if there is no such id.
func (s *Store) GetJob(id string) (*Job, bool, error) {
	row := s.db.QueryRow(`
        SELECT id, type, state, info_hash, params, progress_bytes, total_bytes, error, created_at, updated_at
        FROM jobs WHERE id=?`, id)
	j, err := scanJob(row)
	if err == sql.ErrNoRows {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	return j, true, nil
}

// ListJobs returns jobs newest first. A zero limit means all of them.
func (s *Store) ListJobs(limit int) ([]*Job, error) {
	q := `SELECT id, type, state, info_hash, params, progress_bytes, total_bytes, error, created_at, updated_at
	      FROM jobs ORDER BY created_at DESC`
	args := []interface{}{}
	if limit > 0 {
		q += ` LIMIT ?`
		args = append(args, limit)
	}
	rows, err := s.db.Query(q, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Job
	for rows.Next() {
		j, err := scanJob(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, j)
	}
	return out, rows.Err()
}

// UnfinishedJobs returns the jobs that were mid-flight when the process died.
//
// A job left in `running` or `verifying` by a crash is not resumed blindly:
// the job type decides whether its own partial work can be picked up, because
// only it knows what half a move looks like on disk.
func (s *Store) UnfinishedJobs() ([]*Job, error) {
	rows, err := s.db.Query(`
        SELECT id, type, state, info_hash, params, progress_bytes, total_bytes, error, created_at, updated_at
        FROM jobs WHERE state IN ('pending','running','verifying') ORDER BY created_at`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Job
	for rows.Next() {
		j, err := scanJob(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, j)
	}
	return out, rows.Err()
}

// ActiveJobForTorrent finds a non-terminal job already touching this torrent.
// Two moves of the same payload at once would race on the same bytes.
func (s *Store) ActiveJobForTorrent(infoHash string) (*Job, bool, error) {
	row := s.db.QueryRow(`
        SELECT id, type, state, info_hash, params, progress_bytes, total_bytes, error, created_at, updated_at
        FROM jobs WHERE info_hash=? AND state IN ('pending','running','verifying')
        ORDER BY created_at DESC LIMIT 1`, infoHash)
	j, err := scanJob(row)
	if err == sql.ErrNoRows {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	return j, true, nil
}

// PruneJobs drops finished jobs older than the cutoff, so the table does not
// grow without bound once workflows start firing moves on their own.
func (s *Store) PruneJobs(olderThan time.Time) (int64, error) {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	res, err := s.db.Exec(
		`DELETE FROM jobs WHERE state IN ('done','failed','cancelled') AND updated_at < ?`,
		olderThan.Unix())
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

func scanJob(sc scanner) (*Job, error) {
	var j Job
	var st string
	var created, updated int64
	if err := sc.Scan(&j.ID, &j.Type, &st, &j.InfoHash, &j.Params,
		&j.ProgressBytes, &j.TotalBytes, &j.Error, &created, &updated); err != nil {
		return nil, err
	}
	j.State = JobState(st)
	j.CreatedAt = time.Unix(created, 0)
	j.UpdatedAt = time.Unix(updated, 0)
	return &j, nil
}
