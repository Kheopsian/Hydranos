package move

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeFile(t *testing.T, path string, size int) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	buf := make([]byte, size)
	for i := range buf {
		buf[i] = byte(i % 251)
	}
	if err := os.WriteFile(path, buf, 0o644); err != nil {
		t.Fatal(err)
	}
}

// A release directory with a couple of files, the shape a real payload has.
func release(t *testing.T, root string) {
	t.Helper()
	writeFile(t, filepath.Join(root, "video.mkv"), 4096)
	writeFile(t, filepath.Join(root, "subs", "en.srt"), 128)
}

func TestInspectCountsEveryFile(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	release(t, src)

	p, err := Inspect(src, filepath.Join(base, "dst"))
	if err != nil {
		t.Fatal(err)
	}
	if len(p.Files) != 2 {
		t.Fatalf("expected 2 files, got %d: %v", len(p.Files), p.Files)
	}
	if p.TotalBytes != 4096+128 {
		t.Fatalf("total bytes = %d, want %d", p.TotalBytes, 4096+128)
	}
	if !p.SameFS {
		t.Fatal("two directories in the same temp dir must be on one filesystem")
	}
}

func TestInspectSpotsHardlinks(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	release(t, src)
	// What Sonarr does: the library entry is a second link to the payload.
	lib := filepath.Join(base, "library", "video.mkv")
	if err := os.MkdirAll(filepath.Dir(lib), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(filepath.Join(src, "video.mkv"), lib); err != nil {
		t.Skipf("hardlinks unavailable here: %v", err)
	}

	p, err := Inspect(src, filepath.Join(base, "dst"))
	if err != nil {
		t.Fatal(err)
	}
	if p.HardlinkedFiles != 1 {
		t.Fatalf("expected 1 hardlinked file, got %d", p.HardlinkedFiles)
	}
	if p.HardlinkedBytes != 4096 {
		t.Fatalf("hardlinked bytes = %d, want 4096", p.HardlinkedBytes)
	}
}

func TestCheckAllowsHardlinksOnSameFilesystem(t *testing.T) {
	// A rename keeps the inode, so the link survives and no permission is
	// needed. Refusing here would block the common case for no reason.
	p := &Plan{SameFS: true, HardlinkedFiles: 3}
	if err := p.Check(false); err != nil {
		t.Fatalf("same-fs move must not need hardlink permission: %v", err)
	}
}

func TestCheckRefusesToBreakHardlinksWithoutPermission(t *testing.T) {
	p := &Plan{
		SameFS:          false,
		Files:           []string{"a", "b"},
		HardlinkedFiles: 1,
		HardlinkedBytes: 4096,
		FreeBytes:       1 << 40,
	}
	err := p.Check(false)
	if !errors.Is(err, ErrWouldBreakHardlinks) {
		t.Fatalf("expected ErrWouldBreakHardlinks, got %v", err)
	}
	if err := p.Check(true); err != nil {
		t.Fatalf("explicit permission must allow it: %v", err)
	}
}

func TestCheckRefusesWhenTargetIsTooSmall(t *testing.T) {
	p := &Plan{SameFS: false, TotalBytes: 1 << 30, FreeBytes: 1 << 20}
	if err := p.Check(true); !errors.Is(err, ErrNotEnoughSpace) {
		t.Fatalf("expected ErrNotEnoughSpace, got %v", err)
	}
}

func TestInspectRefusesTargetInsideSource(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	release(t, src)
	if _, err := Inspect(src, filepath.Join(src, "nested")); err == nil {
		t.Fatal("moving a directory into itself must be refused")
	}
}

func TestExecuteRenamesOnSameFilesystem(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	if err := Execute(context.Background(), p, Options{}); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dst, "subs", "en.srt")); err != nil {
		t.Fatalf("payload not at target: %v", err)
	}
	if _, err := os.Stat(src); !os.IsNotExist(err) {
		t.Fatal("source should be gone after a rename")
	}
}

func TestExecuteCopiesAndOnlyThenRemovesSource(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	// Force the copy path even though the temp dirs share a filesystem.
	p.SameFS = false

	var lastProgress int64
	err = Execute(context.Background(), p, Options{
		OnProgress: func(done int64) { lastProgress = done },
	})
	if err != nil {
		t.Fatal(err)
	}
	if lastProgress != p.TotalBytes {
		t.Fatalf("progress ended at %d, want %d", lastProgress, p.TotalBytes)
	}
	got, err := os.ReadFile(filepath.Join(dst, "video.mkv"))
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 4096 {
		t.Fatalf("copied file is %d bytes, want 4096", len(got))
	}
	if _, err := os.Stat(src); !os.IsNotExist(err) {
		t.Fatal("source should be removed once the copy is verified")
	}
	if _, err := os.Stat(dst + stagingSuffix); !os.IsNotExist(err) {
		t.Fatal("staging directory should be gone")
	}
}

func TestCancelledCopyLeavesSourceIntact(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	p.SameFS = false

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	if err := Execute(ctx, p, Options{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("expected cancellation, got %v", err)
	}
	// The whole point: the torrent is still seeding from here.
	if _, err := os.Stat(filepath.Join(src, "video.mkv")); err != nil {
		t.Fatalf("source must survive a cancelled move: %v", err)
	}
	if _, err := os.Stat(dst); !os.IsNotExist(err) {
		t.Fatal("target must not exist after a cancelled move")
	}
	if err := CleanupStaging(dst); err != nil {
		t.Fatalf("staging cleanup: %v", err)
	}
}

func TestResumeSkipsFilesAlreadyCopied(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	p.SameFS = false

	// Pre-populate the staging area as an interrupted attempt would have,
	// but with recognisably different content at the same size. If the file
	// is skipped, that content survives; if it is copied again, it does not.
	// Content is the only honest witness here -- progress numbers depend on
	// directory walk order, which is not what this test is about.
	staging := dst + stagingSuffix
	marker := make([]byte, 4096)
	for i := range marker {
		marker[i] = 0xAB
	}
	if err := os.MkdirAll(staging, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(staging, "video.mkv"), marker, 0o644); err != nil {
		t.Fatal(err)
	}

	var final int64
	if err := Execute(context.Background(), p, Options{
		OnProgress: func(done int64) { final = done },
	}); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(filepath.Join(dst, "video.mkv"))
	if err != nil {
		t.Fatal(err)
	}
	if got[0] != 0xAB {
		t.Fatal("the already-copied file was copied again instead of being reused")
	}
	if final != p.TotalBytes {
		t.Fatalf("progress ended at %d, want %d", final, p.TotalBytes)
	}
	if _, err := os.Stat(filepath.Join(dst, "subs", "en.srt")); err != nil {
		t.Fatalf("remaining file not copied: %v", err)
	}
}

func TestExecuteRefusesWhenTargetExists(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)
	writeFile(t, filepath.Join(dst, "something"), 1)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	if err := Execute(context.Background(), p, Options{}); err == nil {
		t.Fatal("must refuse to move onto an existing target")
	}
	if _, err := os.Stat(filepath.Join(src, "video.mkv")); err != nil {
		t.Fatal("source must be untouched after a refusal")
	}
}

// The production failure this guards against: a cross-filesystem move copied
// 450 MB and only then discovered the target was occupied. The refusal has to
// come from Check, before a single byte is written.
func TestCheckRefusesOccupiedTargetBeforeAnyCopying(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)
	writeFile(t, filepath.Join(dst, "something"), 1)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	if !p.TargetExists || p.TargetEmpty {
		t.Fatalf("Inspect must report an occupied target: exists=%v empty=%v", p.TargetExists, p.TargetEmpty)
	}
	p.SameFS = false // the case that used to copy first and refuse afterwards
	if err := p.Check(true); !errors.Is(err, ErrTargetExists) {
		t.Fatalf("Check error = %v, want ErrTargetExists", err)
	}
	if _, err := os.Stat(dst + stagingSuffix); !os.IsNotExist(err) {
		t.Fatal("nothing may be staged when the plan is refused")
	}
}

// The empty directory an *arr grab leaves at the save path is not an obstacle:
// refusing it would block the ordinary case for nothing.
func TestEmptyTargetDirectoryIsReused(t *testing.T) {
	for _, sameFS := range []bool{true, false} {
		base := t.TempDir()
		src := filepath.Join(base, "src")
		dst := filepath.Join(base, "dst")
		release(t, src)
		if err := os.MkdirAll(dst, 0o755); err != nil {
			t.Fatal(err)
		}

		p, err := Inspect(src, dst)
		if err != nil {
			t.Fatal(err)
		}
		if !p.TargetExists || !p.TargetEmpty {
			t.Fatalf("empty target: exists=%v empty=%v", p.TargetExists, p.TargetEmpty)
		}
		if err := p.Check(true); err != nil {
			t.Fatalf("an empty target must not block the move: %v", err)
		}
		p.SameFS = sameFS
		if err := Execute(context.Background(), p, Options{}); err != nil {
			t.Fatalf("same_fs=%v: %v", sameFS, err)
		}
		got, err := os.ReadFile(filepath.Join(dst, "video.mkv"))
		if err != nil {
			t.Fatalf("same_fs=%v: payload missing from target: %v", sameFS, err)
		}
		if len(got) != 4096 {
			t.Fatalf("same_fs=%v: payload is %d bytes, want 4096", sameFS, len(got))
		}
	}
}

// A target that turned up after Inspect looked is a different story from one
// that was there all along, and the message has to say which.
func TestRefusalSaysWhetherTheTargetWasThereFromTheStart(t *testing.T) {
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)

	p, err := Inspect(src, dst) // target absent at inspection time
	if err != nil {
		t.Fatal(err)
	}
	writeFile(t, filepath.Join(dst, "something"), 1) // ... and occupied afterwards
	err = clearTarget(p)
	if !errors.Is(err, ErrTargetExists) {
		t.Fatalf("error = %v, want ErrTargetExists", err)
	}
	if !strings.Contains(err.Error(), "appeared while") {
		t.Fatalf("error must name the race: %v", err)
	}
	p.TargetExists = true
	if err := clearTarget(p); !strings.Contains(err.Error(), "already there") {
		t.Fatalf("error must not blame a race for a pre-existing target: %v", err)
	}
}
