package pieceio

import (
	"crypto/sha1"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/Kheopsian/hydra/internal/btmeta"
)

// Piece I/O for moving a payload between hosts.
//
// The unit is a whole piece, not an arbitrary byte range, because the torrent
// already carries a SHA-1 per piece. Transferring on that grid means the
// receiver can verify every byte it accepts against the metainfo it already
// has, and that a transfer interrupted at any point resumes from the pieces
// that verified -- no side-channel manifest, no rolling checksums, no trust in
// the transport. It is the one framing where "what do I still need?" has an
// exact answer.

// Context is everything needed to locate a torrent's bytes on one host.
type Context struct {
	layout *btmeta.Layout
	root   string // directory the layout's paths are relative to
}

// New builds a context from a torrent's metainfo and its save path.
func New(torrent []byte, savePath string) (*Context, error) {
	layout, err := btmeta.ParseLayout(torrent)
	if err != nil {
		return nil, fmt.Errorf("piece io: %w", err)
	}
	return &Context{layout: layout, root: savePath}, nil
}

// Layout exposes the parsed metainfo.
func (pc *Context) Layout() *btmeta.Layout { return pc.layout }

func (pc *Context) filePath(i int) string {
	return filepath.Join(append([]string{pc.root}, pc.layout.FilePath(i)...)...)
}

// A short read is an error rather than a short buffer: a piece that is only
// partly on disk is not a piece, and returning what happened to be there would
// hand the far side bytes that cannot hash correctly and blame the network.
// ReadPiece assembles piece i from the files it spans.
func (pc *Context) ReadPiece(i int) ([]byte, error) {
	size := pc.layout.PieceSize(i)
	if size <= 0 {
		return nil, fmt.Errorf("piece io: piece %d out of range", i)
	}
	out := make([]byte, 0, size)
	for _, seg := range pc.layout.PieceSegments(i) {
		f, err := os.Open(pc.filePath(seg.FileIndex))
		if err != nil {
			return nil, fmt.Errorf("piece io: piece %d: %w", i, err)
		}
		buf := make([]byte, seg.Length)
		n, err := readFullAt(f, buf, seg.Offset)
		f.Close()
		if err != nil {
			return nil, fmt.Errorf("piece io: piece %d: %w", i, err)
		}
		if int64(n) != seg.Length {
			return nil, fmt.Errorf("piece io: piece %d: short read (%d of %d bytes)", i, n, seg.Length)
		}
		out = append(out, buf...)
	}
	if int64(len(out)) != size {
		return nil, fmt.Errorf("piece io: piece %d: assembled %d bytes, expected %d", i, len(out), size)
	}
	return out, nil
}

// Verification happens HERE, on the side that will serve the bytes, and not
// only at the sender: a receiver that writes whatever it is handed cannot tell
// a corrupted transfer from a healthy one, and the corruption is only found
// later by a recheck that blames the disk.
// WritePiece verifies data against the metainfo hash for piece i, then writes it.
func (pc *Context) WritePiece(i int, data []byte) error {
	size := pc.layout.PieceSize(i)
	if size <= 0 {
		return fmt.Errorf("piece io: piece %d out of range", i)
	}
	if int64(len(data)) != size {
		return fmt.Errorf("piece io: piece %d: got %d bytes, expected %d", i, len(data), size)
	}
	sum := sha1.Sum(data)
	if want := pc.layout.Pieces[i]; !equalBytes(sum[:], want) {
		return fmt.Errorf("piece io: piece %d failed its hash check", i)
	}

	var off int64
	for _, seg := range pc.layout.PieceSegments(i) {
		path := pc.filePath(seg.FileIndex)
		if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
			return fmt.Errorf("piece io: piece %d: %w", i, err)
		}
		f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE, 0644)
		if err != nil {
			return fmt.Errorf("piece io: piece %d: %w", i, err)
		}
		_, err = f.WriteAt(data[off:off+seg.Length], seg.Offset)
		if cerr := f.Close(); err == nil {
			err = cerr
		}
		if err != nil {
			return fmt.Errorf("piece io: piece %d: %w", i, err)
		}
		off += seg.Length
	}
	return nil
}

func equalBytes(a, b []byte) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// readFullAt fills buf from off, looping over short reads.
func readFullAt(f *os.File, buf []byte, off int64) (int, error) {
	var total int
	for total < len(buf) {
		n, err := f.ReadAt(buf[total:], off+int64(total))
		total += n
		if err != nil {
			if errors.Is(err, os.ErrClosed) || n == 0 {
				return total, err
			}
			return total, err
		}
	}
	return total, nil
}
