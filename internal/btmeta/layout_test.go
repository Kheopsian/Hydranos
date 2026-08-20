package btmeta

import (
	"bytes"
	"fmt"
	"testing"
)

// build a minimal .torrent around a given info dict body. The announce string
// is bencoded rather than written out with a hand-counted length: getting that
// count wrong shifts every following offset, and the parser then reports a
// missing info dict rather than a bad length -- which is exactly how the first
// version of this helper made a negative test pass for the wrong reason.
func benc(s string) string { return fmt.Sprintf("%d:%s", len(s), s) }

func metafile(infoBody string) []byte {
	return []byte("d8:announce" + benc("http://tracker.invalid/announce") + "4:info" + infoBody + "e")
}

func hashes(n int) string {
	var b bytes.Buffer
	fmt.Fprintf(&b, "%d:", n*20)
	for i := 0; i < n*20; i++ {
		b.WriteByte(byte('a' + i%26))
	}
	return b.String()
}

func TestParseLayoutSingleFile(t *testing.T) {
	// The real shape this was written against: 888523 bytes, 16 KiB pieces.
	raw := metafile("d6:lengthi888523e4:name8:some.bin12:piece lengthi16384e6:pieces" + hashes(55) + "e")
	l, err := ParseLayout(raw)
	if err != nil {
		t.Fatalf("ParseLayout: %v", err)
	}
	if l.NumPieces() != 55 {
		t.Errorf("NumPieces = %d, want 55", l.NumPieces())
	}
	if l.TotalSize != 888523 {
		t.Errorf("TotalSize = %d, want 888523", l.TotalSize)
	}
	if len(l.Files) != 1 || l.Files[0].Path[0] != "some.bin" {
		t.Errorf("Files = %+v, want the name as the single file", l.Files)
	}
	// Only the last piece is short.
	if got := l.PieceSize(0); got != 16384 {
		t.Errorf("PieceSize(0) = %d, want 16384", got)
	}
	want := int64(888523 - 54*16384)
	if got := l.PieceSize(54); got != want {
		t.Errorf("PieceSize(54) = %d, want %d", got, want)
	}
	segs := l.PieceSegments(54)
	if len(segs) != 1 || segs[0].FileIndex != 0 || segs[0].Offset != 54*16384 || segs[0].Length != want {
		t.Errorf("PieceSegments(54) = %+v", segs)
	}
}

func TestParseLayoutMultiFileStraddle(t *testing.T) {
	// Two files of 10 bytes with 16-byte pieces: piece 0 straddles the
	// boundary, which is the case that gets offsets wrong when it is wrong.
	raw := metafile("d5:filesld6:lengthi10e4:pathl1:aeed6:lengthi10e4:pathl1:beee4:name3:dir12:piece lengthi16e6:pieces" + hashes(2) + "e")
	l, err := ParseLayout(raw)
	if err != nil {
		t.Fatalf("ParseLayout: %v", err)
	}
	if l.TotalSize != 20 || l.NumPieces() != 2 {
		t.Fatalf("TotalSize=%d NumPieces=%d", l.TotalSize, l.NumPieces())
	}
	segs := l.PieceSegments(0)
	if len(segs) != 2 {
		t.Fatalf("piece 0 should straddle 2 files, got %+v", segs)
	}
	if segs[0] != (Segment{FileIndex: 0, Offset: 0, Length: 10}) {
		t.Errorf("segs[0] = %+v", segs[0])
	}
	if segs[1] != (Segment{FileIndex: 1, Offset: 0, Length: 6}) {
		t.Errorf("segs[1] = %+v", segs[1])
	}
	// Piece 1 is the 4-byte tail of the second file.
	segs = l.PieceSegments(1)
	if len(segs) != 1 || segs[0] != (Segment{FileIndex: 1, Offset: 6, Length: 4}) {
		t.Errorf("PieceSegments(1) = %+v", segs)
	}
}

// A piece count that disagrees with the total size means every offset derived
// from the grid is wrong. It must not parse.
func TestParseLayoutRejectsPieceCountMismatch(t *testing.T) {
	raw := metafile("d6:lengthi100000e4:name3:bin12:piece lengthi16384e6:pieces" + hashes(3) + "e")
	if _, err := ParseLayout(raw); err == nil {
		t.Fatal("expected a mismatch error, got nil")
	}
}

// Every piece's segments must together cover exactly that piece, and the whole
// set must tile the files with no gap and no overlap.
func TestPieceSegmentsTileExactly(t *testing.T) {
	raw := metafile("d5:filesld6:lengthi7e4:pathl1:aeed6:lengthi13e4:pathl1:beed6:lengthi5e4:pathl1:ceee4:name3:dir12:piece lengthi4e6:pieces" + hashes(7) + "e")
	l, err := ParseLayout(raw)
	if err != nil {
		t.Fatalf("ParseLayout: %v", err)
	}
	covered := map[int]int64{}
	for i := 0; i < l.NumPieces(); i++ {
		var sum int64
		for _, s := range l.PieceSegments(i) {
			sum += s.Length
			covered[s.FileIndex] += s.Length
		}
		if sum != l.PieceSize(i) {
			t.Errorf("piece %d: segments cover %d, piece is %d", i, sum, l.PieceSize(i))
		}
	}
	for fi, f := range l.Files {
		if covered[fi] != f.Length {
			t.Errorf("file %d: covered %d of %d bytes", fi, covered[fi], f.Length)
		}
	}
}

// A multi-file torrent with exactly ONE file must still nest under its name.
// This is the case that makes len(Files)==1 the wrong discriminator, so it is
// the case worth pinning.
func TestSingleEntryMultiFileStillNests(t *testing.T) {
	multi := metafile("d5:filesld6:lengthi16e4:pathl5:a.binee" + "e4:name3:dir12:piece lengthi16e6:pieces" + hashes(1) + "e")
	l, err := ParseLayout(multi)
	if err != nil {
		t.Fatalf("ParseLayout: %v", err)
	}
	if !l.MultiFile {
		t.Fatal("MultiFile = false for a torrent that has a files key")
	}
	if len(l.Files) != 1 {
		t.Fatalf("Files = %d, want 1 (the whole point of this case)", len(l.Files))
	}
	got := l.FilePath(0)
	want := []string{"dir", "a.bin"}
	if len(got) != len(want) || got[0] != want[0] || got[1] != want[1] {
		t.Errorf("FilePath(0) = %v, want %v", got, want)
	}

	single := metafile("d6:lengthi16e4:name5:a.bin12:piece lengthi16e6:pieces" + hashes(1) + "e")
	l2, err := ParseLayout(single)
	if err != nil {
		t.Fatalf("ParseLayout single: %v", err)
	}
	if l2.MultiFile {
		t.Fatal("MultiFile = true for a torrent with no files key")
	}
	if got := l2.FilePath(0); len(got) != 1 || got[0] != "a.bin" {
		t.Errorf("FilePath(0) = %v, want [a.bin]", got)
	}
}
