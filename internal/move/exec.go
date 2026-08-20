package move

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// stagingSuffix names the half-built copy. It sits next to the final target
// rather than at it, so nothing ever observes a partially populated content
// directory and mistakes it for the real thing -- including Hydra itself on a
// restart, and any *arr importer watching the folder.
const stagingSuffix = ".hydra-move-partial"

// Options tune a move. The zero value is a sensible move.
type Options struct {
	// AllowBreakingHardlinks is the operator's explicit answer to what
	// Inspect reported. Ignored on a same-filesystem move.
	AllowBreakingHardlinks bool

	// BytesPerSecond caps copy throughput. Zero means no cap. A full-speed
	// copy off the array while a few hundred thousand torrents seed from it
	// is felt by the seeding, so this exists; whether to use it is the
	// operator's call.
	BytesPerSecond int64

	// OnProgress is called with the cumulative byte count. Called often
	// enough to drive a progress bar and rarely enough not to hammer the
	// database: once per file and every 32 MiB within a large one.
	OnProgress func(done int64)

	// BeforeSwap runs after the copy is verified and before the payload is
	// swapped into place. This is where the caller stops the torrent -- and
	// the reason it is a hook rather than something the caller does around
	// Execute: on a cross-filesystem move the torrent must keep seeding for
	// the whole copy, which can be hours, and stop only for the instant the
	// swap takes. Returning an error aborts the move with the source still
	// intact.
	BeforeSwap func() error

	// AfterSwap runs once the payload is at the target and before the old
	// copy is removed. This is where the caller points the engine at the new
	// path and restarts the torrent. If it returns an error the source is
	// left alone, so the torrent can be pointed back at it.
	AfterSwap func() error
}

// Execute performs the move described by the plan.
//
// The order is the whole design. On one filesystem it is a rename, which is
// atomic and keeps every hardlink. Across filesystems it copies into a staging
// directory, verifies what landed, swaps it into place, and only then removes
// the source -- so an interruption at any point leaves the payload intact
// where the torrent is still seeding from it, plus a staging directory that
// the next attempt reuses or that can simply be deleted.
//
// It never removes the source before the destination is verified. That is the
// one rule this function exists to keep.
func Execute(ctx context.Context, p *Plan, opts Options) error {
	if err := p.Check(opts.AllowBreakingHardlinks); err != nil {
		return err
	}

	if p.SameFS {
		if err := os.MkdirAll(filepath.Dir(p.Target), 0o755); err != nil {
			return fmt.Errorf("move: mkdir %s: %w", filepath.Dir(p.Target), err)
		}
		if _, err := os.Stat(p.Target); err == nil {
			return fmt.Errorf("move: target already exists: %s", p.Target)
		}
		if opts.BeforeSwap != nil {
			if err := opts.BeforeSwap(); err != nil {
				return fmt.Errorf("move: preparing the swap: %w", err)
			}
		}
		err := os.Rename(p.Source, p.Target)
		if err == nil {
			if opts.AfterSwap != nil {
				if err := opts.AfterSwap(); err != nil {
					return fmt.Errorf("move: payload is at %s but the engine was not updated: %w", p.Target, err)
				}
			}
			if opts.OnProgress != nil {
				opts.OnProgress(p.TotalBytes)
			}
			return nil
		}
		if !isCrossDevice(err) {
			return fmt.Errorf("move: rename %s -> %s: %w", p.Source, p.Target, err)
		}
		// A rename that stat said should work. Measured on the real setup,
		// this is rarer than it first looked: renaming an ordinary directory
		// between two bind mounts of one filesystem succeeds, so bind mounts
		// alone do not cause it. The case that produced EXDEV in production
		// was a source that was itself a mount root, and Inspect now refuses
		// those outright.
		//
		// The fallback stays because stat cannot promise a rename in general
		// -- overlay and network filesystems have their own rules -- and
		// because the alternative is failing a move that a copy would have
		// completed. It is insurance, not the main path.
		//
		// The operator was told this was a rename, so hardlinks were never
		// raised. A copy breaks them, and that answer has to be asked for
		// rather than assumed.
		if p.HardlinkedFiles > 0 && !opts.AllowBreakingHardlinks {
			return fmt.Errorf("%w: %d of %d files are hardlinked (%d bytes), e.g. %s -- "+
				"the target turned out to be a separate mount, so this has to be a copy, "+
				"which leaves a second full copy on disk",
				ErrWouldBreakHardlinks, p.HardlinkedFiles, len(p.Files),
				p.HardlinkedBytes, strings.Join(p.HardlinkExamples, ", "))
		}
		if err := ensureSpace(p); err != nil {
			return err
		}
		// The torrent was stopped for what was meant to be an instant swap.
		// The copy can take hours, so put it back to work first.
		if opts.AfterSwap != nil {
			if err := opts.AfterSwap(); err != nil {
				return fmt.Errorf("move: could not restart the torrent before falling back to a copy: %w", err)
			}
		}
	}

	staging := p.Target + stagingSuffix
	if err := os.MkdirAll(staging, 0o755); err != nil {
		return fmt.Errorf("move: mkdir staging %s: %w", staging, err)
	}

	var done int64
	for _, rel := range p.Files {
		if err := ctx.Err(); err != nil {
			return err
		}
		srcFile := filepath.Join(p.Source, rel)
		dstFile := filepath.Join(staging, rel)

		si, err := os.Stat(srcFile)
		if err != nil {
			return fmt.Errorf("move: stat %s: %w", srcFile, err)
		}
		// Already copied whole by an earlier attempt? Skip it. This is what
		// makes a resumed move cheap instead of starting from zero.
		if di, err := os.Stat(dstFile); err == nil && di.Size() == si.Size() {
			done += si.Size()
			if opts.OnProgress != nil {
				opts.OnProgress(done)
			}
			continue
		}
		if err := os.MkdirAll(filepath.Dir(dstFile), 0o755); err != nil {
			return fmt.Errorf("move: mkdir %s: %w", filepath.Dir(dstFile), err)
		}
		n, err := copyFile(ctx, srcFile, dstFile, si, opts, done)
		if err != nil {
			return err
		}
		done += n
		if opts.OnProgress != nil {
			opts.OnProgress(done)
		}
	}

	// Verify before anything is destroyed. Sizes only: the torrent engine
	// will hash-check the payload if it is ever asked to, and re-hashing
	// hundreds of gigabytes here would double the cost of every move to
	// re-prove something the copy did not have the opportunity to corrupt
	// silently -- a short read returns an error, it does not return wrong
	// bytes.
	for _, rel := range p.Files {
		si, err := os.Stat(filepath.Join(p.Source, rel))
		if err != nil {
			return fmt.Errorf("move: verify: source %s vanished: %w", rel, err)
		}
		di, err := os.Stat(filepath.Join(staging, rel))
		if err != nil {
			return fmt.Errorf("move: verify: %s missing from copy: %w", rel, err)
		}
		if si.Size() != di.Size() {
			return fmt.Errorf("move: verify: %s is %d bytes at source, %d in copy",
				rel, si.Size(), di.Size())
		}
	}

	if err := os.MkdirAll(filepath.Dir(p.Target), 0o755); err != nil {
		return fmt.Errorf("move: mkdir %s: %w", filepath.Dir(p.Target), err)
	}
	if _, err := os.Stat(p.Target); err == nil {
		return fmt.Errorf("move: target appeared during the copy: %s", p.Target)
	}
	// The copy is done and verified; everything up to here was free to abort.
	// Only now is it worth interrupting the torrent, and only for as long as
	// a rename and an engine call take.
	if opts.BeforeSwap != nil {
		if err := opts.BeforeSwap(); err != nil {
			return fmt.Errorf("move: preparing the swap: %w", err)
		}
	}
	if err := os.Rename(staging, p.Target); err != nil {
		return fmt.Errorf("move: swap staging into place: %w", err)
	}
	if opts.AfterSwap != nil {
		if err := opts.AfterSwap(); err != nil {
			// The payload is at the target and the source still exists, so
			// the torrent can be pointed back at either. Leave both.
			return fmt.Errorf("move: payload is at %s but the engine was not updated: %w", p.Target, err)
		}
	}

	// The destination is verified and in place; the source is now redundant.
	if err := os.RemoveAll(p.Source); err != nil {
		// Not fatal: the move succeeded, the payload is where it should be.
		// Leaving the old copy behind wastes space, which is a complaint, not
		// a data loss.
		return fmt.Errorf("move: completed, but the old copy at %s could not be removed: %w", p.Source, err)
	}
	return nil
}

// ensureSpace re-runs the free-space test for a move that only discovered it
// was a copy once the rename had already failed.
func ensureSpace(p *Plan) error {
	free := freeSpace(p.Target)
	if free <= 0 {
		return nil
	}
	need := p.TotalBytes + p.TotalBytes/100 + (64 << 20)
	if free < need {
		return fmt.Errorf("%w: need %d bytes, %d free on %s", ErrNotEnoughSpace, need, free, p.Target)
	}
	return nil
}

// CleanupStaging removes a half-finished copy. Used when a move is cancelled
// or abandoned; safe to call when there is nothing there.
func CleanupStaging(target string) error {
	err := os.RemoveAll(target + stagingSuffix)
	if os.IsNotExist(err) {
		return nil
	}
	return err
}

func copyFile(ctx context.Context, src, dst string, si os.FileInfo, opts Options, baseDone int64) (int64, error) {
	in, err := os.Open(src)
	if err != nil {
		return 0, fmt.Errorf("move: open %s: %w", src, err)
	}
	defer in.Close()

	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, si.Mode().Perm())
	if err != nil {
		return 0, fmt.Errorf("move: create %s: %w", dst, err)
	}

	buf := make([]byte, 4<<20)
	var written int64
	var sinceReport int64
	limiter := newLimiter(opts.BytesPerSecond)

	for {
		if err := ctx.Err(); err != nil {
			out.Close()
			return written, err
		}
		n, rerr := in.Read(buf)
		if n > 0 {
			if _, werr := out.Write(buf[:n]); werr != nil {
				out.Close()
				return written, fmt.Errorf("move: write %s: %w", dst, werr)
			}
			written += int64(n)
			sinceReport += int64(n)
			limiter.account(ctx, int64(n))
			if sinceReport >= 32<<20 && opts.OnProgress != nil {
				opts.OnProgress(baseDone + written)
				sinceReport = 0
			}
		}
		if rerr == io.EOF {
			break
		}
		if rerr != nil {
			out.Close()
			return written, fmt.Errorf("move: read %s: %w", src, rerr)
		}
	}

	// Flush to disk before this file counts as copied. Without it the
	// verification below could pass against nothing but page cache, and a
	// power cut between the swap and the source deletion would take the
	// payload with it.
	if err := out.Sync(); err != nil {
		out.Close()
		return written, fmt.Errorf("move: sync %s: %w", dst, err)
	}
	if err := out.Close(); err != nil {
		return written, fmt.Errorf("move: close %s: %w", dst, err)
	}
	// Preserve mtime: *arr importers and cross-seed matchers look at it.
	_ = os.Chtimes(dst, time.Now(), si.ModTime())
	return written, nil
}

// parentDir is filepath.Dir with a stable fixed point, so the walk-up loops
// above terminate at the root instead of spinning on ".".
func parentDir(p string) string {
	parent := filepath.Dir(p)
	if parent == p || parent == "." {
		return p
	}
	return parent
}

// limiter is a simple token-per-interval throttle. Deliberately crude: the
// point is to stop a copy from monopolising the array, not to hit a precise
// byte rate.
type limiter struct {
	perSec  int64
	window  time.Time
	inWin   int64
	enabled bool
}

func newLimiter(perSec int64) *limiter {
	return &limiter{perSec: perSec, window: time.Now(), enabled: perSec > 0}
}

func (l *limiter) account(ctx context.Context, n int64) {
	if !l.enabled {
		return
	}
	l.inWin += n
	if l.inWin < l.perSec {
		return
	}
	elapsed := time.Since(l.window)
	if elapsed < time.Second {
		select {
		case <-time.After(time.Second - elapsed):
		case <-ctx.Done():
		}
	}
	l.window = time.Now()
	l.inWin = 0
}
