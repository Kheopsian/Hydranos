package pieceio

import (
	"bytes"
	"crypto/sha1"
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"github.com/Kheopsian/hydra/internal/btmeta"
)

// buildLayout makes a real metafile whose piece hashes match data, so the
// hash check under test is the torrent's own and not a stand-in.
func buildLayout(t *testing.T, data []byte, pieceLen int64, name string) *btmeta.Layout {
	t.Helper()
	var pieces bytes.Buffer
	for off := int64(0); off < int64(len(data)); off += pieceLen {
		end := off + pieceLen
		if end > int64(len(data)) {
			end = int64(len(data))
		}
		sum := sha1.Sum(data[off:end])
		pieces.Write(sum[:])
	}
	info := fmt.Sprintf("d6:lengthi%de4:name%d:%s12:piece lengthi%de6:pieces%d:%s e",
		len(data), len(name), name, pieceLen, pieces.Len(), pieces.String())
	info = info[:len(info)-2] + "e" // drop the space placeholder
	raw := []byte("d4:info" + info + "e")
	l, err := btmeta.ParseLayout(raw)
	if err != nil {
		t.Fatalf("ParseLayout: %v", err)
	}
	return l
}

func TestPieceRoundTrip(t *testing.T) {
	data := make([]byte, 5000)
	for i := range data {
		data[i] = byte(i * 7 % 251)
	}
	l := buildLayout(t, data, 1024, "x.bin")

	srcDir := t.TempDir()
	if err := os.WriteFile(filepath.Join(srcDir, "x.bin"), data, 0644); err != nil {
		t.Fatal(err)
	}
	src := &Context{layout: l, root: srcDir}
	dst := &Context{layout: l, root: t.TempDir()}

	for i := 0; i < l.NumPieces(); i++ {
		p, err := src.ReadPiece(i)
		if err != nil {
			t.Fatalf("readPiece(%d): %v", i, err)
		}
		if err := dst.WritePiece(i, p); err != nil {
			t.Fatalf("writePiece(%d): %v", i, err)
		}
	}
	got, err := os.ReadFile(filepath.Join(dst.root, "x.bin"))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, data) {
		t.Fatalf("round-tripped file differs: %d vs %d bytes", len(got), len(data))
	}
}

// The receiver must reject a piece that does not match the metainfo. Without
// this, a corrupted transfer is written to disk and only surfaces much later,
// as a recheck failure that looks like a bad disk.
func TestWritePieceRejectsCorruption(t *testing.T) {
	data := make([]byte, 3000)
	for i := range data {
		data[i] = byte(i)
	}
	l := buildLayout(t, data, 1024, "y.bin")
	dst := &Context{layout: l, root: t.TempDir()}

	good := data[0:1024]
	bad := append([]byte(nil), good...)
	bad[0] ^= 0xff

	if err := dst.WritePiece(0, bad); err == nil {
		t.Fatal("a corrupted piece was accepted")
	}
	if _, err := os.Stat(filepath.Join(dst.root, "y.bin")); err == nil {
		t.Error("a rejected piece still created the file")
	}
	if err := dst.WritePiece(0, good); err != nil {
		t.Fatalf("the matching piece was rejected: %v", err)
	}
}

// A piece of the wrong length is refused before it is hashed: it can only mean
// the two ends disagree about the grid.
func TestWritePieceRejectsWrongLength(t *testing.T) {
	data := make([]byte, 2048)
	l := buildLayout(t, data, 1024, "z.bin")
	dst := &Context{layout: l, root: t.TempDir()}
	if err := dst.WritePiece(0, make([]byte, 512)); err == nil {
		t.Fatal("a short piece was accepted")
	}
}
