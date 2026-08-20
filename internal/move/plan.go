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

	p := &Plan{Source: src, Target: dst}
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
