package btmeta

import (
	"errors"
	"fmt"
)

// Layout is where a torrent's bytes actually live: the piece grid its hashes
// cover, and the ordered list of files that grid maps onto.
//
// It exists so a transfer can be piece-aligned. BitTorrent already ships a
// SHA-1 for every piece, so a receiver that writes whole pieces can verify what
// it got without any checksum scheme of its own, and can resume from whatever
// it had verified before an interruption. That is why moving bytes between
// agents does not need rsync's rolling checksums: the torrent is the manifest.
type Layout struct {
	Name        string
	PieceLength int64
	Pieces      [][]byte // 20-byte SHA-1 each, in piece order
	Files       []File   // exactly one entry for a single-file torrent
	TotalSize   int64

	// MultiFile records whether the info dict had a `files` key, which is the
	// ONLY correct discriminator for the on-disk layout:
	//   multi-file  -> <root>/<name>/<file path>
	//   single-file -> <root>/<name>
	// It cannot be re-derived from len(Files): a multi-file torrent with a
	// single entry is common, and treating it as single-file points every read
	// at a path that does not exist. Typhon learned this one the hard way
	// (see typhon-engine/src/disk/mod.rs).
	MultiFile bool
}

// File is one file of the torrent, relative to the content root.
type File struct {
	Path   []string // path components; joined by the caller for its own OS
	Length int64
}

// Segment is the part of one file that a piece covers. A piece spans more than
// one segment exactly when it straddles a file boundary.
type Segment struct {
	FileIndex int
	Offset    int64 // byte offset within that file
	Length    int64
}

// ParseLayout reads the info dict of a .torrent.
func ParseLayout(raw []byte) (*Layout, error) {
	info, err := InfoSpan(raw)
	if err != nil {
		return nil, err
	}
	entries, err := dictEntries(info)
	if err != nil {
		return nil, fmt.Errorf("btmeta: info dict: %w", err)
	}

	l := &Layout{}
	var filesVal []byte
	var singleLength int64 = -1
	for _, e := range entries {
		switch e.key {
		case "name":
			s, _, err := scanString(e.val, 0)
			if err != nil {
				return nil, fmt.Errorf("btmeta: name: %w", err)
			}
			l.Name = s
		case "piece length":
			n, err := scanInt(e.val)
			if err != nil {
				return nil, fmt.Errorf("btmeta: piece length: %w", err)
			}
			l.PieceLength = n
		case "pieces":
			s, _, err := scanString(e.val, 0)
			if err != nil {
				return nil, fmt.Errorf("btmeta: pieces: %w", err)
			}
			b := []byte(s)
			if len(b)%20 != 0 {
				return nil, fmt.Errorf("btmeta: pieces blob is %d bytes, not a multiple of 20", len(b))
			}
			for i := 0; i+20 <= len(b); i += 20 {
				l.Pieces = append(l.Pieces, b[i:i+20])
			}
		case "length":
			n, err := scanInt(e.val)
			if err != nil {
				return nil, fmt.Errorf("btmeta: length: %w", err)
			}
			singleLength = n
		case "files":
			filesVal = e.val
		}
	}

	if l.PieceLength <= 0 {
		return nil, errors.New("btmeta: missing or non-positive piece length")
	}
	if len(l.Pieces) == 0 {
		return nil, errors.New("btmeta: no piece hashes")
	}

	switch {
	case filesVal != nil:
		files, err := parseFiles(filesVal)
		if err != nil {
			return nil, err
		}
		l.Files = files
		l.MultiFile = true
	case singleLength >= 0:
		// Single-file torrent: the name IS the file.
		l.Files = []File{{Path: []string{l.Name}, Length: singleLength}}
	default:
		return nil, errors.New("btmeta: info dict has neither length nor files")
	}

	for _, f := range l.Files {
		if f.Length < 0 {
			return nil, errors.New("btmeta: negative file length")
		}
		l.TotalSize += f.Length
	}

	// The piece count must agree with the total size, or the grid the hashes
	// describe is not the grid the files form -- every offset computed from it
	// would then be wrong, silently.
	want := (l.TotalSize + l.PieceLength - 1) / l.PieceLength
	if l.TotalSize == 0 {
		want = 0
	}
	if int64(len(l.Pieces)) != want {
		return nil, fmt.Errorf("btmeta: %d piece hashes for %d bytes at %d per piece (expected %d)",
			len(l.Pieces), l.TotalSize, l.PieceLength, want)
	}
	return l, nil
}

func parseFiles(v []byte) ([]File, error) {
	if len(v) == 0 || v[0] != 'l' {
		return nil, errors.New("btmeta: files is not a list")
	}
	var out []File
	i := 1
	for i < len(v) && v[i] != 'e' {
		end, err := scanValue(v, i)
		if err != nil {
			return nil, err
		}
		entries, err := dictEntries(v[i:end])
		if err != nil {
			return nil, fmt.Errorf("btmeta: file entry: %w", err)
		}
		var f File
		f.Length = -1
		for _, e := range entries {
			switch e.key {
			case "length":
				n, err := scanInt(e.val)
				if err != nil {
					return nil, fmt.Errorf("btmeta: file length: %w", err)
				}
				f.Length = n
			case "path":
				parts, err := parseStringList(e.val)
				if err != nil {
					return nil, fmt.Errorf("btmeta: file path: %w", err)
				}
				f.Path = parts
			}
		}
		if f.Length < 0 {
			return nil, errors.New("btmeta: file entry without a length")
		}
		if len(f.Path) == 0 {
			return nil, errors.New("btmeta: file entry without a path")
		}
		out = append(out, f)
		i = end
	}
	if i >= len(v) {
		return nil, errors.New("btmeta: unterminated files list")
	}
	return out, nil
}

func parseStringList(v []byte) ([]string, error) {
	if len(v) == 0 || v[0] != 'l' {
		return nil, errors.New("btmeta: not a list")
	}
	var out []string
	i := 1
	for i < len(v) && v[i] != 'e' {
		s, next, err := scanString(v, i)
		if err != nil {
			return nil, err
		}
		out = append(out, s)
		i = next
	}
	return out, nil
}

func scanInt(v []byte) (int64, error) {
	if len(v) < 3 || v[0] != 'i' || v[len(v)-1] != 'e' {
		return 0, fmt.Errorf("btmeta: not an int: %q", string(v))
	}
	body := v[1 : len(v)-1]
	neg := false
	if body[0] == '-' {
		neg = true
		body = body[1:]
	}
	if len(body) == 0 {
		return 0, errors.New("btmeta: empty int")
	}
	var n int64
	for _, c := range body {
		if c < '0' || c > '9' {
			return 0, fmt.Errorf("btmeta: bad int digit %q", c)
		}
		n = n*10 + int64(c-'0')
	}
	if neg {
		n = -n
	}
	return n, nil
}

// NumPieces is how many pieces the torrent has.
func (l *Layout) NumPieces() int { return len(l.Pieces) }

// PieceSize is the length of piece i. Only the last piece may be short.
func (l *Layout) PieceSize(i int) int64 {
	if i < 0 || i >= len(l.Pieces) {
		return 0
	}
	start := int64(i) * l.PieceLength
	if rest := l.TotalSize - start; rest < l.PieceLength {
		return rest
	}
	return l.PieceLength
}

// PieceSegments maps piece i onto the files it covers, in order.
func (l *Layout) PieceSegments(i int) []Segment {
	size := l.PieceSize(i)
	if size <= 0 {
		return nil
	}
	start := int64(i) * l.PieceLength
	end := start + size

	var out []Segment
	var cursor int64
	for fi, f := range l.Files {
		fileStart, fileEnd := cursor, cursor+f.Length
		cursor = fileEnd
		if f.Length == 0 || fileEnd <= start {
			continue
		}
		if fileStart >= end {
			break
		}
		from := start
		if fileStart > from {
			from = fileStart
		}
		to := end
		if fileEnd < to {
			to = fileEnd
		}
		out = append(out, Segment{FileIndex: fi, Offset: from - fileStart, Length: to - from})
	}
	return out
}

// FilePath is where file i lives on disk, given the torrent's save path.
// Components are returned for the caller to join with its own separator, so a
// path produced on Linux and one produced on Windows agree on structure.
func (l *Layout) FilePath(i int) []string {
	if i < 0 || i >= len(l.Files) {
		return nil
	}
	if l.MultiFile {
		return append([]string{l.Name}, l.Files[i].Path...)
	}
	return append([]string{}, l.Files[i].Path...)
}
