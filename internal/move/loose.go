package move

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
)

// ErrTargetFileExists is returned when a loose move would land on names that
// are already taken.
//
// A normal move gets a directory of its own, so "is the target free?" is one
// question about one path. A loose payload moves into a directory it shares
// with whatever is already there, so the only honest check is per file -- and
// the answer is a refusal, never an overwrite: the file in the way belongs to
// somebody else's torrent.
var ErrTargetFileExists = errors.New("target already holds files with these names")

// InspectLoose plans the move of a payload that has no folder of its own --
// files sitting directly in a directory shared with other torrents.
//
// Everything that makes this different follows from source not being ours: the
// file list comes from the torrent rather than from walking the directory, the
// collision check is per file rather than about the directory as a whole, and
// source is never renamed and never removed. A mount point is fine here,
// unlike in Inspect, precisely because the directory itself never moves.
//
// key namespaces the staging directory. Two loose moves into one category
// would otherwise assemble their copies in the same place.
func InspectLoose(source, target, key string, rel []string) (*Plan, error) {
	if source == "" || target == "" {
		return nil, fmt.Errorf("move: source and target are both required")
	}
	if len(rel) == 0 {
		return nil, fmt.Errorf("move: a loose move needs the torrent's file list; got none")
	}
	src, err := filepath.Abs(source)
	if err != nil {
		return nil, err
	}
	dst, err := filepath.Abs(target)
	if err != nil {
		return nil, err
	}
	if src == dst {
		return nil, fmt.Errorf("move: source and target are the same path")
	}
	if _, err := os.Stat(src); err != nil {
		return nil, fmt.Errorf("move: source unreadable: %w", err)
	}

	p := &Plan{Source: src, Target: dst, Loose: true}
	p.Staging = filepath.Join(dst, "."+stagingKey(key)+stagingSuffix)

	for _, r := range rel {
		clean, err := safeRel(r)
		if err != nil {
			return nil, err
		}
		info, err := os.Stat(filepath.Join(src, clean))
		if err != nil {
			return nil, fmt.Errorf("move: payload file %s: %w", clean, err)
		}
		if !info.Mode().IsRegular() {
			return nil, fmt.Errorf("move: payload file %s is not a regular file", clean)
		}
		p.Files = append(p.Files, clean)
		p.TotalBytes += info.Size()
		if n := linkCount(info); n > 1 {
			p.HardlinkedFiles++
			p.HardlinkedBytes += info.Size()
			if len(p.HardlinkExamples) < 5 {
				p.HardlinkExamples = append(p.HardlinkExamples, clean)
			}
		}
		if _, err := os.Stat(filepath.Join(dst, clean)); err == nil {
			p.Collisions = append(p.Collisions, clean)
		}
	}

	p.SameFS = sameFilesystem(src, dst)
	p.FreeBytes = freeSpace(dst)
	return p, nil
}

// safeRel rejects anything in a torrent's file list that would write outside
// the directory the payload lives in. A torrent is remote input: its paths are
// whatever the person who made it typed.
func safeRel(r string) (string, error) {
	if r == "" {
		return "", fmt.Errorf("move: empty path in the torrent's file list")
	}
	if filepath.IsAbs(r) {
		return "", fmt.Errorf("move: absolute path %q in the torrent's file list", r)
	}
	clean := filepath.Clean(filepath.FromSlash(r))
	if clean == "." || clean == ".." || strings.HasPrefix(clean, ".."+string(os.PathSeparator)) {
		return "", fmt.Errorf("move: path %q escapes the torrent's directory", r)
	}
	return clean, nil
}

// stagingKey reduces an arbitrary key to something safe to put in a path.
func stagingKey(key string) string {
	var b strings.Builder
	for _, r := range key {
		switch {
		case r >= '0' && r <= '9', r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r == '-':
			b.WriteRune(r)
		}
	}
	if b.Len() == 0 {
		return "hydra"
	}
	return b.String()
}

// executeLoose moves the listed files out of a shared directory.
//
// The shape mirrors Execute -- rename on one filesystem, copy-verify-swap
// across two -- with one rule added: p.Source is a category directory holding
// other torrents, so it is never renamed and never removed. Only p.Files move.
func executeLoose(ctx context.Context, p *Plan, opts Options) error {
	if p.SameFS {
		if opts.BeforeSwap != nil {
			if err := opts.BeforeSwap(); err != nil {
				return fmt.Errorf("move: preparing the swap: %w", err)
			}
		}
		moved, err := looseRename(p)
		if err == nil {
			if opts.AfterSwap != nil {
				if err := opts.AfterSwap(); err != nil {
					return fmt.Errorf("move: payload is at %s but the engine was not updated: %w", p.Target, err)
				}
			}
			if opts.OnProgress != nil {
				opts.OnProgress(p.TotalBytes)
			}
			pruneEmptyDirs(p)
			return nil
		}
		// Put back what had already moved, whatever went wrong. Half a
		// payload in each directory is the one outcome with no good recovery.
		looseUndo(p, moved)
		if !isCrossDevice(err) {
			return err
		}
		// stat said one filesystem and rename disagreed. Two bind mounts of
		// one pool share a device number and still refuse a rename between
		// them, which is the ordinary shape of this container -- so this is
		// expected, not exotic, and it has to become a copy.
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
		// The torrent was stopped for what was meant to be an instant rename.
		// The copy takes as long as it takes, and the payload is back at the
		// source untouched, so put it to work there for the duration.
		if opts.AbortSwap != nil {
			if err := opts.AbortSwap(); err != nil {
				return fmt.Errorf("move: could not restart the torrent before falling back to a copy: %w", err)
			}
		}
	}
	return looseCopy(ctx, p, opts)
}

// looseRename moves each file across, returning what it managed before failing.
func looseRename(p *Plan) ([]string, error) {
	var moved []string
	for _, rel := range p.Files {
		from := filepath.Join(p.Source, rel)
		to := filepath.Join(p.Target, rel)
		if err := os.MkdirAll(filepath.Dir(to), 0o755); err != nil {
			return moved, fmt.Errorf("move: mkdir %s: %w", filepath.Dir(to), err)
		}
		// Re-checked here and not only in Inspect: a plan is a snapshot, and
		// an overwrite would destroy another torrent's file.
		if _, err := os.Stat(to); err == nil {
			return moved, fmt.Errorf("%w: %s", ErrTargetFileExists, to)
		}
		if err := os.Rename(from, to); err != nil {
			return moved, fmt.Errorf("move: rename %s -> %s: %w", from, to, err)
		}
		moved = append(moved, rel)
	}
	return moved, nil
}

// looseUndo puts moved files back at the source, newest first.
//
// Best effort by necessity -- there is nothing better to try if it fails --
// but loud, because a file that will not go back is the one thing here that
// leaves a torrent broken.
func looseUndo(p *Plan, moved []string) {
	for i := len(moved) - 1; i >= 0; i-- {
		rel := moved[i]
		if err := os.Rename(filepath.Join(p.Target, rel), filepath.Join(p.Source, rel)); err != nil {
			slog.Error("move: could not put a file back after a failed loose move",
				"file", rel, "source", p.Source, "target", p.Target, "error", err)
		}
	}
}

// looseCopy copies the payload into a staging directory on the target
// filesystem, verifies it, and only then swaps it in and drops the originals.
func looseCopy(ctx context.Context, p *Plan, opts Options) error {
	staging := p.Staging
	if staging == "" {
		staging = filepath.Join(p.Target, "."+stagingKey("")+stagingSuffix)
	}
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

	if opts.BeforeSwap != nil {
		if err := opts.BeforeSwap(); err != nil {
			return fmt.Errorf("move: preparing the swap: %w", err)
		}
	}
	var swapped []string
	for _, rel := range p.Files {
		to := filepath.Join(p.Target, rel)
		if err := os.MkdirAll(filepath.Dir(to), 0o755); err != nil {
			looseUnswap(p, staging, swapped)
			return fmt.Errorf("move: mkdir %s: %w", filepath.Dir(to), err)
		}
		if _, err := os.Stat(to); err == nil {
			looseUnswap(p, staging, swapped)
			return fmt.Errorf("%w: %s", ErrTargetFileExists, to)
		}
		if err := os.Rename(filepath.Join(staging, rel), to); err != nil {
			looseUnswap(p, staging, swapped)
			return fmt.Errorf("move: swap %s into place: %w", rel, err)
		}
		swapped = append(swapped, rel)
	}
	if opts.AfterSwap != nil {
		if err := opts.AfterSwap(); err != nil {
			return fmt.Errorf("move: payload is at %s but the engine was not updated: %w", p.Target, err)
		}
	}

	// Verified and in place, so the originals are redundant. Only the
	// torrent's own files go: the directory around them is the category.
	for _, rel := range p.Files {
		if err := os.Remove(filepath.Join(p.Source, rel)); err != nil && !os.IsNotExist(err) {
			return fmt.Errorf("move: completed, but the old copy of %s could not be removed: %w", rel, err)
		}
	}
	pruneEmptyDirs(p)
	_ = os.RemoveAll(staging)
	return nil
}

// looseUnswap returns already-swapped files to staging so a failed swap leaves
// the target as it was found.
func looseUnswap(p *Plan, staging string, swapped []string) {
	for i := len(swapped) - 1; i >= 0; i-- {
		rel := swapped[i]
		if err := os.Rename(filepath.Join(p.Target, rel), filepath.Join(staging, rel)); err != nil {
			slog.Error("move: could not undo a partial loose swap",
				"file", rel, "target", p.Target, "error", err)
		}
	}
}

// pruneEmptyDirs removes directories the payload left behind under Source.
//
// Only empty ones, only strictly below Source, and never Source itself: it is
// the shared category directory. Failures are ignored -- a leftover empty
// directory is untidy, not wrong.
func pruneEmptyDirs(p *Plan) {
	seen := map[string]bool{}
	for _, rel := range p.Files {
		dir := filepath.Dir(filepath.Join(p.Source, rel))
		for dir != p.Source && strings.HasPrefix(dir, p.Source+string(os.PathSeparator)) {
			if seen[dir] {
				break
			}
			seen[dir] = true
			if os.Remove(dir) != nil {
				break
			}
			dir = parentDir(dir)
		}
	}
}
