package jobs

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"log/slog"
	"time"

	"github.com/Kheopsian/hydra/internal/btmeta"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/store"
)

// PieceSource is the node that currently holds the payload.
type PieceSource interface {
	ExportState(infoHash string) (*ltclient.ResumeRecord, error)
	GetTorrentFile(infoHash string) ([]byte, error)
	ReadPiece(infoHash string, piece int) ([]byte, error)
	// Release is what turns a duplicate into a move.
	StopTorrent(infoHash string) error
	RemoveTorrent(infoHash string, keepData bool) error
}

// PieceSink is the node that will hold it afterwards.
type PieceSink interface {
	ExportState(infoHash string) (*ltclient.ResumeRecord, error)
	ImportStateWithFile(rec *ltclient.ResumeRecord, torrent []byte) (string, error)
	WritePiece(infoHash string, piece int, data []byte) error
	VerifyTorrent(infoHash string) error
	StartTorrent(infoHash string) error
}

// Neither interface has Close, deliberately. The connections these resolve to
// are the long-lived, shared agent clients the front already keeps open: a job
// that closed one when it finished would tear down the connection every other
// caller is using. Connection lifetime belongs to whoever dialled.

// RemoteMoveParams is the durable description of a cross-node move.
type RemoteMoveParams struct {
	// Agents are named, not addressed. Job params are persisted in the
	// database, so putting an addr and a bearer token in here would write the
	// agent's credentials to disk in clear, once per job, forever. A name is
	// resolved against the running agent registry at execution time instead.
	SourceAgent string `json:"source_agent"`
	TargetAgent string `json:"target_agent"`
	// Engine is the engine id on both ends (e.g. "hoard").
	Engine string `json:"engine"`
	// Category is where the payload lands: the target's save path is the one
	// that category defines FOR THAT AGENT. Deliberately not a free-text path
	// -- a path typed for one host is meaningless on another, and the category
	// already holds the per-agent mapping. Changing category is a separate
	// action, before or after.
	Category string `json:"category"`
	// ReleaseSource distinguishes the two operations the UI offers: false
	// duplicates (both nodes keep it), true moves (the source is released once
	// the target is verified and running, never before).
	ReleaseSource bool `json:"release_source"`
	// KeepSourceData releases the torrent but leaves its files behind.
	KeepSourceData bool `json:"keep_source_data,omitempty"`
	// Name is carried for readability; the job outlives the torrent.
	Name string `json:"name,omitempty"`
	// BytesPerSecond caps the transfer. Zero is uncapped.
	BytesPerSecond int64 `json:"bytes_per_second,omitempty"`
}

// RemoteMoveRunner carries a torrent and its bytes to another node.
//
// It never removes anything from the source. The source keeps its copy and
// keeps seeding for the whole transfer, so a failure at any point leaves the
// working copy exactly where it was; releasing the source is a separate,
// deliberate act once the destination is known good.
type RemoteMoveRunner struct {
	DialSource func(p RemoteMoveParams) (PieceSource, error)
	DialSink   func(p RemoteMoveParams) (PieceSink, error)
	// ResolveSavePath gives the save path a category defines for an agent.
	// Supplied by the front, which owns the category model; the runner stays
	// out of it. It must REFUSE rather than fall back to a local default: a
	// Linux path handed to a Windows agent lands the payload nowhere, silently.
	ResolveSavePath func(agent, category string) (string, error)
	// FreeSpace reports the bytes available at a path on an agent, for the
	// preflight. Returning an error skips the check rather than failing the
	// job: not knowing is not the same as knowing there is no room.
	FreeSpace func(agent, path string) (int64, error)
}

func (r *RemoteMoveRunner) Type() string { return store.JobTypeMoveDataRemote }

// Resume: always resumable, and cheaply so. The destination's bitfield is the
// progress record -- it survives a crash on either side because it is derived
// from bytes on disk that were hash-checked before they were written. There is
// no staging state to reconcile and nothing to undo.
func (r *RemoteMoveRunner) Resume(j *store.Job) (bool, string) {
	var p RemoteMoveParams
	if err := json.Unmarshal([]byte(j.Params), &p); err != nil {
		return false, "job params unreadable: " + err.Error()
	}
	return true, ""
}

func (r *RemoteMoveRunner) Run(ctx context.Context, j *store.Job, report func(done, total int64)) error {
	var p RemoteMoveParams
	if err := json.Unmarshal([]byte(j.Params), &p); err != nil {
		return fmt.Errorf("remote move: unreadable params: %w", err)
	}
	if p.Category == "" {
		return fmt.Errorf("remote move: category is required")
	}
	targetSavePath, err := r.ResolveSavePath(p.TargetAgent, p.Category)
	if err != nil {
		return fmt.Errorf("remote move: %w", err)
	}

	src, err2 := r.DialSource(p)
	err = err2
	if err != nil {
		return fmt.Errorf("remote move: source agent %q: %w", p.SourceAgent, err)
	}
	dst, err := r.DialSink(p)
	if err != nil {
		return fmt.Errorf("remote move: target agent %q: %w", p.TargetAgent, err)
	}

	rec, err := src.ExportState(j.InfoHash)
	if err != nil {
		return fmt.Errorf("remote move: export: %w", err)
	}
	blob, err := src.GetTorrentFile(j.InfoHash)
	if err != nil {
		return fmt.Errorf("remote move: .torrent: %w", err)
	}
	layout, err := btmeta.ParseLayout(blob)
	if err != nil {
		return fmt.Errorf("remote move: %w", err)
	}
	report(0, layout.TotalSize)

	// Preflight: refuse for lack of room BEFORE moving a byte, not at piece
	// 40,000. Only the bytes still missing are needed, so a resumed job is not
	// asked for room it already filled.
	if r.FreeSpace != nil {
		if free, ferr := r.FreeSpace(p.TargetAgent, targetSavePath); ferr == nil && free >= 0 {
			if free < layout.TotalSize {
				return fmt.Errorf("remote move: %s needs %d bytes at %q, only %d free",
					p.TargetAgent, layout.TotalSize, targetSavePath, free)
			}
		} else if ferr != nil {
			slog.Warn("remote move: could not check free space, continuing",
				"agent", p.TargetAgent, "path", targetSavePath, "err", ferr)
		}
	}

	// Adopt only on a fresh run. On a resumed one the destination already holds
	// the torrent, and re-adopting would discard the bitfield that says how far
	// the previous attempt got.
	if _, gErr := dst.ExportState(j.InfoHash); gErr != nil {
		adopt := *rec
		adopt.SavePath = targetSavePath
		// A destination that has not received a byte must not claim the
		// source's progress: a torrent that believes it is complete will serve
		// zeros. Paused for the same reason -- it is a shell until the bytes
		// land.
		adopt.Bitfield = ""
		adopt.Paused = true
		adopt.SeedMode = false
		if _, aErr := dst.ImportStateWithFile(&adopt, blob); aErr != nil {
			return fmt.Errorf("remote move: adopt on target: %w", aErr)
		}
		slog.Info("remote move: adopted on target, paused",
			"info_hash", j.InfoHash, "target", p.TargetAgent, "save_path", targetSavePath)
	} else {
		slog.Info("remote move: target already holds it, resuming from its bitfield",
			"info_hash", j.InfoHash, "target", p.TargetAgent)
	}

	present, err := presentPieces(dst, j.InfoHash, layout.NumPieces())
	if err != nil {
		return fmt.Errorf("remote move: %w", err)
	}

	var done int64
	for i := 0; i < layout.NumPieces(); i++ {
		if present[i] {
			done += layout.PieceSize(i)
		}
	}
	report(done, layout.TotalSize)

	throttle := newThrottle(p.BytesPerSecond)
	for i := 0; i < layout.NumPieces(); i++ {
		if err := ctx.Err(); err != nil {
			return err
		}
		if present[i] {
			continue
		}
		data, err := src.ReadPiece(j.InfoHash, i)
		if err != nil {
			return fmt.Errorf("remote move: reading piece %d: %w", i, err)
		}
		if err := dst.WritePiece(j.InfoHash, i, data); err != nil {
			// The target hashes before it writes, so this is a real mismatch,
			// not a warning to carry on past.
			return fmt.Errorf("remote move: writing piece %d: %w", i, err)
		}
		done += int64(len(data))
		report(done, layout.TotalSize)
		throttle.took(int64(len(data)))
	}

	// The bytes are all hash-checked individually; the recheck is what turns
	// them into the target engine's own bitfield and lets it call itself
	// complete.
	if err := dst.VerifyTorrent(j.InfoHash); err != nil {
		return fmt.Errorf("remote move: recheck on target: %w", err)
	}
	if err := dst.StartTorrent(j.InfoHash); err != nil {
		return fmt.Errorf("remote move: starting on target: %w", err)
	}

	if !p.ReleaseSource {
		slog.Info("remote move: duplicated, both nodes now hold it",
			"info_hash", j.InfoHash, "bytes", layout.TotalSize, "target", p.TargetAgent)
		return nil
	}

	// Release LAST, and only now: the target has the bytes, has hashed them,
	// and is running. Up to this point every failure left the source as the
	// working copy, which is the whole discipline of this ordering.
	if err := src.StopTorrent(j.InfoHash); err != nil {
		return fmt.Errorf("remote move: target is live but the source could not be stopped: %w", err)
	}
	if err := src.RemoveTorrent(j.InfoHash, p.KeepSourceData); err != nil {
		return fmt.Errorf("remote move: target is live but the source could not be released: %w", err)
	}
	slog.Info("remote move: moved, source released",
		"info_hash", j.InfoHash, "bytes", layout.TotalSize,
		"target", p.TargetAgent, "kept_source_data", p.KeepSourceData)
	return nil
}

// presentPieces asks the destination what it already has. That answer is the
// resume point and the only one consulted.
func presentPieces(dst PieceSink, infoHash string, n int) (map[int]bool, error) {
	rec, err := dst.ExportState(infoHash)
	if err != nil {
		return nil, fmt.Errorf("reading target bitfield: %w", err)
	}
	out := make(map[int]bool, n)
	if rec.Bitfield == "" {
		return out, nil
	}
	raw, err := hex.DecodeString(rec.Bitfield)
	if err != nil {
		return nil, fmt.Errorf("target bitfield is not hex: %w", err)
	}
	for i := 0; i < n; i++ {
		if i/8 < len(raw) && raw[i/8]>>(7-uint(i%8))&1 == 1 {
			out[i] = true
		}
	}
	return out, nil
}

// throttle paces the transfer so a move does not eat the link the torrents are
// being served on.
type throttle struct {
	bps  int64
	last time.Time
}

func newThrottle(bps int64) *throttle { return &throttle{bps: bps, last: time.Now()} }

func (t *throttle) took(n int64) {
	if t.bps <= 0 {
		return
	}
	want := time.Duration(float64(n) / float64(t.bps) * float64(time.Second))
	if elapsed := time.Since(t.last); elapsed < want {
		time.Sleep(want - elapsed)
	}
	t.last = time.Now()
}
