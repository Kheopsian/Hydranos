package ltclient

import (
	"bufio"
	"bytes"
	"strings"
	"testing"
)

// A frame must survive the prealloc path byte for byte. This is the test that
// matters: readFrame feeds every RPC reply and every event, so a truncated or
// spliced frame would not be a slow path, it would be a broken daemon.
func TestReadFramePreallocPreservesBytes(t *testing.T) {
	small := `{"id":1,"result":"ok"}`
	big := `{"id":2,"result":"` + strings.Repeat("x", 40000) + `"}`
	huge := `{"id":3,"result":"` + strings.Repeat("y", 90000) + `"}`
	wire := small + "\n" + big + "\n" + huge + "\n" + small + "\n"

	for _, prealloc := range []bool{false, true} {
		optPrealloc.Store(prealloc)
		frameHint.Store(0)
		// A deliberately tiny buffer so every large frame goes through the
		// ErrBufferFull path several times.
		r := bufio.NewReaderSize(bytes.NewReader([]byte(wire)), 64)
		for i, want := range []string{small, big, huge, small} {
			got, err := readFrame(r)
			if err != nil {
				t.Fatalf("prealloc=%v frame %d: %v", prealloc, i, err)
			}
			if string(got) != want {
				t.Fatalf("prealloc=%v frame %d: got %d bytes, want %d",
					prealloc, i, len(got), len(want))
			}
		}
	}
	optPrealloc.Store(false)
	frameHint.Store(0)
}

// The hint must grow to cover the largest frame seen, and never shrink back to
// a size that would put us back on the doubling cascade.
func TestFrameHintGrows(t *testing.T) {
	optPrealloc.Store(true)
	frameHint.Store(0)
	defer func() { optPrealloc.Store(false); frameHint.Store(0) }()

	big := `{"id":1,"d":"` + strings.Repeat("z", 50000) + `"}`
	r := bufio.NewReaderSize(bytes.NewReader([]byte(big+"\n")), 64)
	if _, err := readFrame(r); err != nil {
		t.Fatal(err)
	}
	if got := frameHint.Load(); got < int64(len(big)) {
		t.Fatalf("hint %d did not reach frame size %d", got, len(big))
	}
}

// With the flag off the hint must stay untouched, so the OFF rung of a
// measurement ladder really is the old behaviour.
func TestPreallocOffLeavesHintAlone(t *testing.T) {
	optPrealloc.Store(false)
	frameHint.Store(0)
	big := `{"id":1,"d":"` + strings.Repeat("q", 30000) + `"}`
	r := bufio.NewReaderSize(bytes.NewReader([]byte(big+"\n")), 64)
	if _, err := readFrame(r); err != nil {
		t.Fatal(err)
	}
	if got := frameHint.Load(); got != 0 {
		t.Fatalf("hint moved to %d with the flag off", got)
	}
}
