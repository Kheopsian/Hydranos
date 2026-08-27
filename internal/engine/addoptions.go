package engine

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// AddOptions carries the per-add overrides the operator can set in the UI's
// "Add a torrent" form. Both are opt-in: the zero value reproduces exactly what
// the add path did before they existed (daemon-wide create_torrent_folder, full
// hash check).
type AddOptions struct {
	// CreateFolder overrides daemon.create_torrent_folder for THIS add. nil
	// means "use the daemon default". It only changes anything for a
	// single-file torrent: a multi-file one always carries its own folder.
	CreateFolder *bool
	// SkipRecheck adds the torrent in seed_mode: the engine trusts the payload
	// already on disk instead of hashing it. Before doing so the add verifies
	// that every file the torrent declares is really there, at the right size,
	// and fails the add otherwise -- a seed_mode add over missing data does not
	// re-download it, it seeds a lie.
	SkipRecheck bool
}

// WrapFolder reports whether this add should put a single-file payload in its
// own folder, given the daemon-wide default.
func (o AddOptions) WrapFolder(daemonDefault bool) bool {
	if o.CreateFolder != nil {
		return *o.CreateFolder
	}
	return daemonDefault
}

// torrentFile is one entry of a torrent's file list, with the BEP-3 relative
// path already joined.
type torrentFile struct {
	Path   string
	Length int64
}

// torrentFileList returns the payload name and file list of raw .torrent bytes.
// It is a real bencode walk (not the byte-scanning shortcuts used by
// nameFromTorrentFile and friends) because the file list is nested and its
// lengths have to be exact.
func torrentFileList(data []byte) (name string, multi bool, files []torrentFile, err error) {
	top, _, err := bencValue(data, 0)
	if err != nil {
		return "", false, nil, err
	}
	root, ok := top.(map[string]interface{})
	if !ok {
		return "", false, nil, errors.New("torrent: top level is not a dict")
	}
	info, ok := root["info"].(map[string]interface{})
	if !ok {
		return "", false, nil, errors.New("torrent: no info dict")
	}
	nameB, _ := info["name"].([]byte)
	name = string(nameB)
	if name == "" {
		return "", false, nil, errors.New("torrent: info dict has no name")
	}
	if raw, ok := info["files"].([]interface{}); ok {
		multi = true
		for _, entry := range raw {
			fd, ok := entry.(map[string]interface{})
			if !ok {
				continue
			}
			segs, ok := fd["path"].([]interface{})
			if !ok {
				continue
			}
			parts := make([]string, 0, len(segs))
			for _, s := range segs {
				if b, ok := s.([]byte); ok {
					parts = append(parts, string(b))
				}
			}
			if len(parts) == 0 {
				continue
			}
			length, _ := fd["length"].(int64)
			files = append(files, torrentFile{Path: filepath.Join(parts...), Length: length})
		}
		if len(files) == 0 {
			return "", false, nil, errors.New("torrent: empty file list")
		}
		return name, true, files, nil
	}
	length, ok := info["length"].(int64)
	if !ok {
		return "", false, nil, errors.New("torrent: neither files nor length")
	}
	return name, false, []torrentFile{{Path: name, Length: length}}, nil
}

// verifyPayloadPresent checks that the payload a seed_mode add is about to
// trust is really on disk under engineSavePath, laid out the way Typhon writes
// it: <engine save_path>/<info.name if multi-file>/<BEP-3 path>.
//
// This is the whole point of "skip the recheck": without it a wrong save path
// is accepted silently and the torrent sits at 100% serving read errors, which
// is worse than the re-download it was meant to avoid.
func verifyPayloadPresent(torrentBytes []byte, engineSavePath string) error {
	name, multi, files, err := torrentFileList(torrentBytes)
	if err != nil {
		return fmt.Errorf("skip-recheck: cannot read the file list: %w", err)
	}
	root := engineSavePath
	if multi {
		root = filepath.Join(engineSavePath, name)
	}
	var missing []string
	var wrongSize []string
	for _, f := range files {
		full := filepath.Join(root, f.Path)
		st, serr := os.Stat(full)
		switch {
		case serr != nil:
			missing = append(missing, f.Path)
		case st.IsDir():
			missing = append(missing, f.Path)
		case st.Size() != f.Length:
			wrongSize = append(wrongSize, fmt.Sprintf("%s (%d bytes on disk, %d expected)", f.Path, st.Size(), f.Length))
		}
	}
	if len(missing) == 0 && len(wrongSize) == 0 {
		return nil
	}
	var b strings.Builder
	fmt.Fprintf(&b, "skip-recheck refused: the data is not at %s", root)
	if len(missing) > 0 {
		fmt.Fprintf(&b, "; %d/%d file(s) missing: %s", len(missing), len(files), summarisePaths(missing))
	}
	if len(wrongSize) > 0 {
		fmt.Fprintf(&b, "; %d file(s) with the wrong size: %s", len(wrongSize), summarisePaths(wrongSize))
	}
	return errors.New(b.String())
}

// summarisePaths keeps an error message readable for a 5000-file torrent.
func summarisePaths(p []string) string {
	const max = 3
	if len(p) <= max {
		return strings.Join(p, ", ")
	}
	return strings.Join(p[:max], ", ") + fmt.Sprintf(", and %d more", len(p)-max)
}

// ---------------------------------------------------------------------------
// Minimal bencode reader (int64 | []byte | []interface{} | map[string]interface{})
// ---------------------------------------------------------------------------

func bencValue(b []byte, i int) (interface{}, int, error) {
	if i >= len(b) {
		return nil, i, errors.New("bencode: unexpected end")
	}
	switch c := b[i]; {
	case c == 'i':
		end := bytes.IndexByte(b[i:], 'e')
		if end < 0 {
			return nil, i, errors.New("bencode: unterminated int")
		}
		n, err := strconv.ParseInt(string(b[i+1:i+end]), 10, 64)
		if err != nil {
			return nil, i, fmt.Errorf("bencode: bad int: %w", err)
		}
		return n, i + end + 1, nil
	case c >= '0' && c <= '9':
		colon := bytes.IndexByte(b[i:], ':')
		if colon < 0 {
			return nil, i, errors.New("bencode: unterminated string length")
		}
		n, err := strconv.Atoi(string(b[i : i+colon]))
		if err != nil || n < 0 {
			return nil, i, errors.New("bencode: bad string length")
		}
		start := i + colon + 1
		if start+n > len(b) {
			return nil, i, errors.New("bencode: string past end")
		}
		return b[start : start+n], start + n, nil
	case c == 'l':
		i++
		var out []interface{}
		for {
			if i >= len(b) {
				return nil, i, errors.New("bencode: unterminated list")
			}
			if b[i] == 'e' {
				return out, i + 1, nil
			}
			v, next, err := bencValue(b, i)
			if err != nil {
				return nil, i, err
			}
			out = append(out, v)
			i = next
		}
	case c == 'd':
		i++
		out := map[string]interface{}{}
		for {
			if i >= len(b) {
				return nil, i, errors.New("bencode: unterminated dict")
			}
			if b[i] == 'e' {
				return out, i + 1, nil
			}
			k, next, err := bencValue(b, i)
			if err != nil {
				return nil, i, err
			}
			kb, ok := k.([]byte)
			if !ok {
				return nil, i, errors.New("bencode: non-string dict key")
			}
			v, next2, err := bencValue(b, next)
			if err != nil {
				return nil, i, err
			}
			out[string(kb)] = v
			i = next2
		}
	default:
		return nil, i, fmt.Errorf("bencode: unexpected %q at %d", c, i)
	}
}
