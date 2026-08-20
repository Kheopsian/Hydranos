package move

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"syscall"
	"testing"
)

func TestInspectRefusesAFilesystemRoot(t *testing.T) {
	// Found in production: a torrent whose content root was /calewood, a whole
	// bind-mounted share. Moving it would have relocated and then deleted the
	// entire volume. The filesystem root is the same class of mistake and is
	// the one case a test can reach without mounting anything.
	if _, err := Inspect("/", t.TempDir()); err == nil {
		t.Fatal("moving a filesystem root must be refused")
	}
}

func TestIsCrossDeviceRecognisesEXDEVThroughLinkError(t *testing.T) {
	// os.Rename wraps the errno in *os.LinkError, which is what the fallback
	// actually receives. Matching only a bare errno would miss every real
	// case.
	err := &os.LinkError{
		Op:  "rename",
		Old: "/calewood",
		New: "/config/tr-data/movies/calewood",
		Err: syscall.EXDEV,
	}
	if !isCrossDevice(err) {
		t.Fatal("EXDEV inside a LinkError must be recognised as cross-device")
	}
	if isCrossDevice(&os.LinkError{Op: "rename", Err: syscall.EACCES}) {
		t.Fatal("a permission error is not a cross-device error")
	}
	if isCrossDevice(errors.New("some other failure")) {
		t.Fatal("an unrelated error is not a cross-device error")
	}
}

func TestSameFsMoveStillWorksAfterTheProbeChange(t *testing.T) {
	// The rename is now a probe rather than a certainty; the ordinary
	// same-filesystem case must still take it and finish instantly.
	base := t.TempDir()
	src := filepath.Join(base, "src")
	dst := filepath.Join(base, "dst")
	release(t, src)

	p, err := Inspect(src, dst)
	if err != nil {
		t.Fatal(err)
	}
	if !p.SameFS {
		t.Skip("temp dirs are not on one filesystem here")
	}
	var swapped, restarted bool
	err = Execute(context.Background(), p, Options{
		BeforeSwap: func() error { swapped = true; return nil },
		AfterSwap:  func() error { restarted = true; return nil },
	})
	if err != nil {
		t.Fatal(err)
	}
	if !swapped || !restarted {
		t.Fatal("both hooks must run on the rename path")
	}
	if _, err := os.Stat(filepath.Join(dst, "video.mkv")); err != nil {
		t.Fatalf("payload not at target: %v", err)
	}
}
