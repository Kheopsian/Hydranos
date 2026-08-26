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
	// StartTorrent puts the source back the way it was. Only a handoff needs
	// it: that is the one path that stops the source BEFORE the target holds
	// anything, so a failure in between has to be undone.
	StartTorrent(infoHash string) error
	RemoveTorrent(infoHash string, keepData bool) error
}

// PieceSink is the node that will hold it afterwards.
type PieceSink interface {
	ExportState(infoHash string) (*ltclient.ResumeRecord, error)
	ImportStateWithFile(rec *ltclient.ResumeRecord, torrent []byte) (string, error)
	WritePiece(infoHash string, piece int, data []byte) error
	VerifyTorrent(infoHash string) error
	StartTorrent(infoHash string) error
	// Undoing an adopt this run created, when the transfer then fails.
	RemoveTorrent(infoHash string, keepData bool) error
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
	// Handoff means both ends are engines of the SAME node and the payload is
	// already at the path the target expects, so there is nothing to transfer:
	// the torrent changes hands where it lies.
	//
	// This is not an optimisation, it is a correctness requirement. Copying
	// would have the target write the very files the source is reading, and the
	// release at the end would delete the payload the target now points at.
	Handoff bool `json:"handoff,omitempty"`
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
	// SetTargetCategory labels the torrent on the destination once it is live.
	//
	// The category is Hydra's own layer: it is in neither the resume record nor
	// the torrent status, so it does not travel with the payload. Without this
	// the torrent lands correctly on disk and shows up with no category at all,
	// which reads as data loss even though nothing was lost. Supplied by the
	// front, which knows how to reach a local engine or a remote one; nil skips.
	SetTargetCategory func(p RemoteMoveParams, infoHash string) error
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

	if p.Handoff {
		return r.handoff(j, p, src, dst, rec, blob, targetSavePath, layout.TotalSize, report)
	}

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

	// Tracks whether THIS run created the target torrent, so a failure can undo
	// exactly what it made. A resumed run must not remove a shell an earlier
	// run left with real pieces already in it.
	adoptedHere := false

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
		adoptedHere = true
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
	// A failure after the adopt must not leave a torrent on the target with no
	// data behind it: it shows up in the list as a real torrent that holds
	// nothing, which is worse than no trace at all.
	undoAdopt := func(cause error) error {
		if adoptedHere {
			if rErr := dst.RemoveTorrent(j.InfoHash, false); rErr != nil {
				slog.Warn("remote move: failed and the target shell could not be removed",
					"info_hash", j.InfoHash, "target", p.TargetAgent, "err", rErr)
			}
		}
		return cause
	}

	for i := 0; i < layout.NumPieces(); i++ {
		if err := ctx.Err(); err != nil {
			return undoAdopt(err)
		}
		if present[i] {
			continue
		}
		data, err := src.ReadPiece(j.InfoHash, i)
		if err != nil {
			return undoAdopt(fmt.Errorf("remote move: reading piece %d: %w", i, err))
		}
		if err := dst.WritePiece(j.InfoHash, i, data); err != nil {
			// The target hashes before it writes, so this is a real mismatch,
			// not a warning to carry on past.
			return undoAdopt(fmt.Errorf("remote move: writing piece %d: %w", i, err))
		}
		done += int64(len(data))
		report(done, layout.TotalSize)
		throttle.took(int64(len(data)))
	}

	// The bytes are all hash-checked individually; the recheck is what turns
	// them into the target engine's own bitfield and lets it call itself
	// complete.
	if err := dst.VerifyTorrent(j.InfoHash); err != nil {
		return undoAdopt(fmt.Errorf("remote move: recheck on target: %w", err))
	}
	if err := dst.StartTorrent(j.InfoHash); err != nil {
		return fmt.Errorf("remote move: starting on target: %w", err)
	}
	// Label it, but never fail a delivered payload over a label: the bytes are
	// there and verified, and a missing category is fixable from the UI.
	if r.SetTargetCategory != nil {
		if cErr := r.SetTargetCategory(p, j.InfoHash); cErr != nil {
			slog.Warn("remote move: payload delivered but the category could not be set",
				"info_hash", j.InfoHash, "category", p.Category, "target", p.TargetAgent, "err", cErr)
		}
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

// handoff gives a torrent to another engine of this node without moving a byte.
//
// The two engines share a filesystem and the payload is already at the path the
// target expects, so the transfer path is not merely wasteful here, it is
// destructive: the target would write the files the source is reading, and the
// release at the end would delete what the target now points at.
//
// The order is the whole safety argument. The source stops FIRST, so the files
// are never open for writing by two engines at once -- the opposite of the copy
// path, where the source keeps seeding until the target is proven good. That
// costs a window where nobody is seeding, seconds long, and it is the only
// ordering that cannot corrupt the payload.
//
// The source's bitfield is carried over rather than rechecked. It describes the
// same bytes on the same disk, hash-checked when they were written; rechecking
// would read the whole payload to learn what the record already says.
func (r *RemoteMoveRunner) handoff(j *store.Job, p RemoteMoveParams,
	src PieceSource, dst PieceSink, rec *ltclient.ResumeRecord, blob []byte,
	targetSavePath string, totalSize int64, report func(done, total int64)) error {

	// A handoff is only safe when the payload really is where the target will
	// look. If the two paths differ, this is a copy, and calling it a handoff
	// would hand the target an empty directory it believes is complete.
	if rec.SavePath != targetSavePath {
		return fmt.Errorf("remote move: handoff needs one path, source is at %q and the target expects %q",
			rec.SavePath, targetSavePath)
	}
	if !p.ReleaseSource {
		// Two engines seeding one set of files is not a duplicate, it is two
		// writers on the same bytes the first time either repairs a piece.
		return fmt.Errorf("remote move: a handoff cannot duplicate -- both engines would hold the same files")
	}
	if _, gErr := dst.ExportState(j.InfoHash); gErr == nil {
		return fmt.Errorf("remote move: the target engine already holds this torrent")
	}

	if err := src.StopTorrent(j.InfoHash); err != nil {
		return fmt.Errorf("remote move: handoff: stopping the source: %w", err)
	}
	restoreSource := func(cause error) error {
		if sErr := src.StartTorrent(j.InfoHash); sErr != nil {
			slog.Error("remote move: handoff failed and the source could not be restarted, it is stopped",
				"info_hash", j.InfoHash, "source", p.SourceAgent, "err", sErr)
		}
		return cause
	}

	adopt := *rec
	adopt.SavePath = targetSavePath
	adopt.Paused = true
	if _, aErr := dst.ImportStateWithFile(&adopt, blob); aErr != nil {
		return restoreSource(fmt.Errorf("remote move: handoff: adopt on target: %w", aErr))
	}
	if sErr := dst.StartTorrent(j.InfoHash); sErr != nil {
		if rErr := dst.RemoveTorrent(j.InfoHash, true); rErr != nil {
			slog.Warn("remote move: handoff: the target shell could not be removed",
				"info_hash", j.InfoHash, "target", p.TargetAgent, "err", rErr)
		}
		return restoreSource(fmt.Errorf("remote move: handoff: starting on target: %w", sErr))
	}
	if r.SetTargetCategory != nil {
		if cErr := r.SetTargetCategory(p, j.InfoHash); cErr != nil {
			slog.Warn("remote move: handoff: delivered but the category could not be set",
				"info_hash", j.InfoHash, "category", p.Category, "target", p.TargetAgent, "err", cErr)
		}
	}
	// keepData is TRUE and not a choice: the files are the target's now. This
	// is the one call in this file where the wrong argument destroys the
	// payload it was asked to move.
	if err := src.RemoveTorrent(j.InfoHash, true); err != nil {
		return fmt.Errorf("remote move: handoff: the target holds it but the source could not be released: %w", err)
	}
	report(totalSize, totalSize)
	slog.Info("remote move: handed over in place, no bytes moved",
		"info_hash", j.InfoHash, "source", p.SourceAgent, "target", p.TargetAgent, "save_path", targetSavePath)
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
