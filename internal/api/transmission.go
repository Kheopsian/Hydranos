package api

import (
	"crypto/sha1"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Reading a Transmission install without talking to it.
//
// Transmission has no way to hand over a .torrent: the RPC exposes the path of
// the file, never its contents, and the piece hashes appear nowhere in the API
// (the "pieces" field is a have-bitfield, not the hashes). So an import reads
// its config folder instead -- which turns out to be better than the RPC
// anyway, because it also works when Transmission is already stopped or
// uninstalled, which is how most migrations actually go.
//
// The folder holds everything needed:
//
//	torrents/  the metainfo, one .torrent per torrent
//	resume/    one bencoded .resume per torrent: destination, paused, uploaded,
//	           downloaded, added_date, labels, seeding_time_seconds
//
// Filenames are NOT trusted to identify anything: the naming scheme changed
// across versions (hash only in 3.00, name + partial hash before and after), so
// every .torrent is parsed and its infohash computed.

// bencode ---------------------------------------------------------------

// bencVal is a decoded bencode value: int64, []byte, []bencVal or map[string]bencVal.
type bencVal interface{}

type bencReader struct {
	buf []byte
	pos int
}

func (r *bencReader) byteAt() (byte, error) {
	if r.pos >= len(r.buf) {
		return 0, errors.New("bencode: unexpected end")
	}
	return r.buf[r.pos], nil
}

func (r *bencReader) value() (bencVal, error) {
	c, err := r.byteAt()
	if err != nil {
		return nil, err
	}
	switch {
	case c == 'i':
		return r.integer()
	case c == 'l':
		return r.list()
	case c == 'd':
		return r.dict()
	case c >= '0' && c <= '9':
		return r.str()
	}
	return nil, fmt.Errorf("bencode: unexpected %q at %d", c, r.pos)
}

func (r *bencReader) integer() (int64, error) {
	end := strings.IndexByte(string(r.buf[r.pos:]), 'e')
	if end < 0 {
		return 0, errors.New("bencode: unterminated int")
	}
	var n int64
	var neg bool
	s := r.buf[r.pos+1 : r.pos+end]
	for i, ch := range s {
		if i == 0 && ch == '-' {
			neg = true
			continue
		}
		if ch < '0' || ch > '9' {
			return 0, errors.New("bencode: bad int")
		}
		n = n*10 + int64(ch-'0')
	}
	r.pos += end + 1
	if neg {
		n = -n
	}
	return n, nil
}

func (r *bencReader) str() ([]byte, error) {
	colon := -1
	for i := r.pos; i < len(r.buf); i++ {
		if r.buf[i] == ':' {
			colon = i
			break
		}
		if r.buf[i] < '0' || r.buf[i] > '9' {
			return nil, errors.New("bencode: bad string length")
		}
	}
	if colon < 0 {
		return nil, errors.New("bencode: unterminated string")
	}
	n := 0
	for _, ch := range r.buf[r.pos:colon] {
		n = n*10 + int(ch-'0')
	}
	start := colon + 1
	if start+n > len(r.buf) || n < 0 {
		return nil, errors.New("bencode: string past end")
	}
	r.pos = start + n
	return r.buf[start : start+n], nil
}

func (r *bencReader) list() ([]bencVal, error) {
	r.pos++ // 'l'
	out := []bencVal{}
	for {
		c, err := r.byteAt()
		if err != nil {
			return nil, err
		}
		if c == 'e' {
			r.pos++
			return out, nil
		}
		v, err := r.value()
		if err != nil {
			return nil, err
		}
		out = append(out, v)
	}
}

func (r *bencReader) dict() (map[string]bencVal, error) {
	r.pos++ // 'd'
	out := map[string]bencVal{}
	for {
		c, err := r.byteAt()
		if err != nil {
			return nil, err
		}
		if c == 'e' {
			r.pos++
			return out, nil
		}
		k, err := r.str()
		if err != nil {
			return nil, err
		}
		v, err := r.value()
		if err != nil {
			return nil, err
		}
		out[string(k)] = v
	}
}

func bencDecode(b []byte) (map[string]bencVal, error) {
	r := &bencReader{buf: b}
	v, err := r.value()
	if err != nil {
		return nil, err
	}
	d, ok := v.(map[string]bencVal)
	if !ok {
		return nil, errors.New("bencode: top level is not a dict")
	}
	return d, nil
}

func bencInt(d map[string]bencVal, key string) int64 {
	if v, ok := d[key].(int64); ok {
		return v
	}
	return 0
}

func bencStr(d map[string]bencVal, key string) string {
	if v, ok := d[key].([]byte); ok {
		return string(v)
	}
	return ""
}

func bencStrList(d map[string]bencVal, key string) []string {
	l, ok := d[key].([]bencVal)
	if !ok {
		return nil
	}
	out := make([]string, 0, len(l))
	for _, it := range l {
		if b, ok := it.([]byte); ok && len(b) > 0 {
			out = append(out, string(b))
		}
	}
	return out
}

// .torrent --------------------------------------------------------------

type torrentMeta struct {
	InfoHash  string
	Name      string
	TotalSize int64
	MultiFile bool
}

// parseTorrentMeta reads a .torrent and computes its infohash, which is the
// SHA1 of the RAW bytes of the info dict -- so the dict is located in the
// original buffer rather than re-encoded (re-encoding would change the hash
// for any file whose key order is not canonical).
func parseTorrentMeta(b []byte) (torrentMeta, error) {
	var m torrentMeta
	start, end, err := infoDictRange(b)
	if err != nil {
		return m, err
	}
	sum := sha1.Sum(b[start:end])
	m.InfoHash = hex.EncodeToString(sum[:])

	top, err := bencDecode(b)
	if err != nil {
		return m, err
	}
	info, _ := top["info"].(map[string]bencVal)
	if info == nil {
		return m, errors.New("torrent: no info dict")
	}
	m.Name = bencStr(info, "name")
	if length := bencInt(info, "length"); length > 0 {
		m.TotalSize = length
	} else if files, ok := info["files"].([]bencVal); ok {
		m.MultiFile = true
		for _, f := range files {
			if fd, ok := f.(map[string]bencVal); ok {
				m.TotalSize += bencInt(fd, "length")
			}
		}
	}
	return m, nil
}

// infoDictRange returns the byte range of the top-level "info" dict.
func infoDictRange(b []byte) (int, int, error) {
	r := &bencReader{buf: b}
	if c, err := r.byteAt(); err != nil || c != 'd' {
		return 0, 0, errors.New("torrent: not a bencoded dict")
	}
	r.pos++
	for {
		c, err := r.byteAt()
		if err != nil {
			return 0, 0, err
		}
		if c == 'e' {
			return 0, 0, errors.New("torrent: no info dict")
		}
		k, err := r.str()
		if err != nil {
			return 0, 0, err
		}
		start := r.pos
		if _, err := r.value(); err != nil {
			return 0, 0, err
		}
		if string(k) == "info" {
			return start, r.pos, nil
		}
	}
}

// .resume ---------------------------------------------------------------

// resumeInfo is what Transmission remembers about a torrent besides its
// metainfo. Field names are the bencode keys, verified against
// libtransmission/resume.cc.
type resumeInfo struct {
	Destination   string
	IncompleteDir string
	Paused        bool
	Uploaded      int64
	Downloaded    int64
	AddedDate     int64
	DoneDate      int64
	SeedingTime   int64
	Labels        []string
	Name          string
}

func parseResume(b []byte) (resumeInfo, error) {
	var ri resumeInfo
	d, err := bencDecode(b)
	if err != nil {
		return ri, err
	}
	ri.Destination = bencStr(d, "destination")
	ri.IncompleteDir = bencStr(d, "incomplete-dir")
	if ri.IncompleteDir == "" {
		ri.IncompleteDir = bencStr(d, "incomplete_dir")
	}
	ri.Paused = bencInt(d, "paused") != 0
	ri.Uploaded = bencInt(d, "uploaded")
	ri.Downloaded = bencInt(d, "downloaded")
	ri.AddedDate = bencInt(d, "added-date")
	if ri.AddedDate == 0 {
		ri.AddedDate = bencInt(d, "added_date")
	}
	ri.DoneDate = bencInt(d, "done-date")
	if ri.DoneDate == 0 {
		ri.DoneDate = bencInt(d, "done_date")
	}
	ri.SeedingTime = bencInt(d, "seeding-time-seconds")
	if ri.SeedingTime == 0 {
		ri.SeedingTime = bencInt(d, "seeding_time_seconds")
	}
	ri.Labels = bencStrList(d, "labels")
	ri.Name = bencStr(d, "name")
	return ri, nil
}

// folder scan -----------------------------------------------------------

// transmissionTorrent is one torrent ready to import: its metainfo, and
// whatever Transmission remembered about it.
type transmissionTorrent struct {
	Meta        torrentMeta
	Resume      resumeInfo
	HasResume   bool
	TorrentPath string
}

// scanTransmissionDir reads a Transmission config folder. Accepts either the
// config root (with torrents/ and resume/ inside) or the torrents/ folder
// itself -- people point at both, and guessing wrong is a confusing failure.
func scanTransmissionDir(root string) ([]transmissionTorrent, []string, error) {
	torrentsDir := filepath.Join(root, "torrents")
	resumeDir := filepath.Join(root, "resume")
	if st, err := os.Stat(torrentsDir); err != nil || !st.IsDir() {
		// Pointed straight at torrents/ ? Then resume/ is its sibling.
		torrentsDir = root
		resumeDir = filepath.Join(filepath.Dir(root), "resume")
	}
	entries, err := os.ReadDir(torrentsDir)
	if err != nil {
		return nil, nil, fmt.Errorf("no torrents folder at %s: %w", torrentsDir, err)
	}

	var out []transmissionTorrent
	var problems []string
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(strings.ToLower(e.Name()), ".torrent") {
			continue
		}
		p := filepath.Join(torrentsDir, e.Name())
		b, err := os.ReadFile(p)
		if err != nil {
			problems = append(problems, e.Name()+": "+err.Error())
			continue
		}
		meta, err := parseTorrentMeta(b)
		if err != nil {
			problems = append(problems, e.Name()+": "+err.Error())
			continue
		}
		tt := transmissionTorrent{Meta: meta, TorrentPath: p}
		// The .resume file shares the .torrent's base name. Fall back to the
		// infohash spelling used by Transmission 3.00.
		base := strings.TrimSuffix(e.Name(), filepath.Ext(e.Name()))
		for _, cand := range []string{base + ".resume", meta.InfoHash + ".resume"} {
			rb, err := os.ReadFile(filepath.Join(resumeDir, cand))
			if err != nil {
				continue
			}
			if ri, err := parseResume(rb); err == nil {
				tt.Resume = ri
				tt.HasResume = true
				break
			}
		}
		out = append(out, tt)
	}
	return out, problems, nil
}

// savePathFor picks where the data actually lives: an incomplete torrent may
// still be sitting in Transmission's incomplete_dir rather than its final
// destination, and adopting it at the wrong place would restart the download.
func (t transmissionTorrent) savePathFor(complete bool) string {
	if !complete && t.Resume.IncompleteDir != "" {
		return t.Resume.IncompleteDir
	}
	return t.Resume.Destination
}
