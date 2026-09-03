// Package move relocates a torrent's payload files.
//
// The hard part is not the copying. It is that a torrent is being seeded from
// those bytes while they move, that a cross-filesystem move is a copy and so
// can half-succeed, and that the *arr stack hardlinks the same data into its
// own library -- a hardlink a rename preserves and a copy silently breaks,
// leaving two full copies on disk where the operator expected one.
//
// So the work is split in two. Inspect() looks and reports; nothing it does
// changes anything on disk, and its findings are what the UI turns into a
// "this will break N hardlinks, continue?" prompt. Execute() acts, and only
// after being told explicitly that the findings are acceptable.
package move

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// ErrWouldBreakHardlinks is returned by Check when the move crosses a
// filesystem boundary and some of the payload is hardlinked elsewhere. It is a
// distinct error because the caller is expected to surface it as a choice
// rather than a failure: a checkbox in a workflow, a modal by hand.
var ErrWouldBreakHardlinks = errors.New("move would break hardlinks")

// ErrNotEnoughSpace is returned when the target filesystem cannot hold the
// payload. Checked up front because discovering it two hours into a copy of a
// 400 GB release is the worst possible moment.
var ErrNotEnoughSpace = errors.New("not enough free space on target")

// ErrTargetExists is returned when something already occupies the target path.
// Checked by Inspect, so a cross-filesystem move is refused before the copy
// starts rather than after it: the old code only looked once the bytes were
// already written, which burned the whole copy to reach a refusal that was
// knowable up front.
//
// An EMPTY target directory is not this error. The *arr stack routinely leaves
// one behind (a grab creates the save path before any data arrives), and
// refusing that would block the ordinary case for no reason -- Execute removes
// it and renames into its place.
var ErrTargetExists = errors.New("target already exists")

// Plan is what Inspect found. It is a snapshot, not a promise: free space can
// change under a long copy, which is why Execute re-checks nothing it cannot
// afford to be wrong about.
type Plan struct {
	Source string
	Target string

	// Files is every regular file under Source, relative to it.
	Files      []string
	TotalBytes int64

	// SameFS means the move is a rename: instantaneous, and hardlinks
	// survive because the inodes never change.
	SameFS bool

	// HardlinkedFiles counts payload files with more than one link. On a
	// same-filesystem move this is harmless information; on a copy it is
	// the number of *arr hardlinks that will be broken, each one turning
	// into a second full copy of that file on disk.
	HardlinkedFiles  int
	HardlinkedBytes  int64
	HardlinkExamples []string

	FreeBytes int64

	// TargetExists means something is already at Target. TargetEmpty
	// narrows it to the harmless case: an empty directory, which Execute
	// removes on its way past.
	TargetExists bool
	TargetEmpty  bool

	// Loose marks a payload with no folder of its own: its files sit
	// directly in a directory shared with other torrents. Source is then
	// that shared directory, and only Files may leave it -- Source itself
	// is never renamed and never removed.
	Loose bool

	// Collisions are the relative paths already occupied under Target. Only
	// a loose move can have them: a normal move lands in a directory of its
	// own, so the whole-directory check covers it.
	Collisions []string

	// Staging is where a cross-filesystem copy is assembled before being
	// swapped into place. It is always on the target's filesystem.
	Staging string
}

// Inspect walks the payload and reports what moving it would involve. It reads
// only metadata -- no file contents -- so it is cheap even on a large release.
func Inspect(source, target string) (*Plan, error) {
	if source == "" || target == "" {
		return nil, fmt.Errorf("move: source and target are both required")
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
	// Refuse to move a directory into itself: the walk would chase its own
	// output forever and the payload would end up nested in itself.
	if strings.HasPrefix(dst+string(os.PathSeparator), src+string(os.PathSeparator)) {
		return nil, fmt.Errorf("move: target %s is inside source %s", dst, src)
	}
	if _, err := os.Stat(src); err != nil {
		return nil, fmt.Errorf("move: source unreadable: %w", err)
	}
	// A content root that is itself a mount is not a release, it is a volume.
	// Seen in production: a torrent whose save_path was /calewood, whose move
	// would have relocated and then deleted the entire share.
	if isMountPoint(src) {
		return nil, fmt.Errorf("move: %s is a mount point, not a torrent folder; "+
			"moving it would move the whole volume", src)
	}

	p := &Plan{Source: src, Target: dst, Staging: dst + stagingSuffix}
	if st, err := os.Stat(dst); err == nil {
		p.TargetExists = true
		p.TargetEmpty = st.IsDir() && dirIsEmpty(dst)
	}
	err = filepath.WalkDir(src, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		if !info.Mode().IsRegular() {
			// Symlinks and device nodes are not torrent payload. Skipping
			// them beats copying something that is not ours to copy.
			return nil
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		p.Files = append(p.Files, rel)
		p.TotalBytes += info.Size()
		if n := linkCount(info); n > 1 {
			p.HardlinkedFiles++
			p.HardlinkedBytes += info.Size()
			if len(p.HardlinkExamples) < 5 {
				p.HardlinkExamples = append(p.HardlinkExamples, rel)
			}
		}
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("move: walking %s: %w", src, err)
	}

	p.SameFS = sameFilesystem(src, dst)
	p.FreeBytes = freeSpace(dst)
	return p, nil
}

// Check decides whether the plan may proceed.
//
// allowBreakingHardlinks is the operator's explicit answer to what Inspect
// found, and is consulted only when it actually matters: a same-filesystem
// move preserves hardlinks, so it never needs permission.
func (p *Plan) Check(allowBreakingHardlinks bool) error {
	// Before anything else, and on both paths: a rename onto an occupied
	// path fails, and a copy onto one wastes hours to fail the same way.
	//
	// A loose payload is the exception: it moves into a directory it shares
	// with other torrents, so that directory being occupied is the normal
	// case and the only meaningful question is per file.
	if p.Loose {
		if len(p.Collisions) > 0 {
			return fmt.Errorf("%w: %s already holds %s",
				ErrTargetFileExists, p.Target, strings.Join(p.Collisions, ", "))
		}
	} else if p.TargetExists && !p.TargetEmpty {
		return fmt.Errorf("%w: %s", ErrTargetExists, p.Target)
	}
	if p.SameFS {
		return nil
	}
	// Only a copy consumes space. A margin over the exact size covers
	// filesystem overhead and the fact that other things keep writing to the
	// same pool while a long copy runs.
	if p.FreeBytes > 0 {
		need := p.TotalBytes + p.TotalBytes/100 + (64 << 20)
		if p.FreeBytes < need {
			return fmt.Errorf("%w: need %d bytes, %d free on %s",
				ErrNotEnoughSpace, need, p.FreeBytes, p.Target)
		}
	}
	if p.HardlinkedFiles > 0 && !allowBreakingHardlinks {
		return fmt.Errorf("%w: %d of %d files are hardlinked (%d bytes), e.g. %s -- "+
			"copying them to another filesystem leaves a second full copy on disk",
			ErrWouldBreakHardlinks, p.HardlinkedFiles, len(p.Files),
			p.HardlinkedBytes, strings.Join(p.HardlinkExamples, ", "))
	}
	return nil
}

// dirIsEmpty reports whether dir holds no entries. An unreadable directory is
// reported as non-empty: refusing a move is recoverable, clearing a directory
// we could not read is not.
func dirIsEmpty(dir string) bool {
	f, err := os.Open(dir)
	if err != nil {
		return false
	}
	defer f.Close()
	names, err := f.Readdirnames(1)
	if err != nil && err != io.EOF {
		return false
	}
	return len(names) == 0
}
