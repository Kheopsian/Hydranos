package api

import (
	"crypto/sha1"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// buildTorrent writes a minimal single-file .torrent and returns its true
// infohash, computed here the same way a tracker would: SHA1 over the raw
// bytes of the info dict.
func buildTorrent(t *testing.T, name string, length int) ([]byte, string) {
	t.Helper()
	pieces := make([]byte, 20) // one fake piece hash
	// Keys deliberately NOT in the order parseTorrentMeta reads them, to make
	// sure the hash comes from the raw bytes and not from a re-encoding.
	info := "d" + bs("length") + bi(length) + bs("name") + bs(name) +
		bs("piece length") + bi(16384) + bs("pieces") + bs(string(pieces)) + "e"
	full := "d" + bs("announce") + bs("http://tracker.test") + bs("info") + info + "e"
	sum := sha1.Sum([]byte(info))
	return []byte(full), hex.EncodeToString(sum[:])
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var b []byte
	for n > 0 {
		b = append([]byte{byte('0' + n%10)}, b...)
		n /= 10
	}
	return string(b)
}

func TestParseTorrentMetaInfoHash(t *testing.T) {
	raw, want := buildTorrent(t, "movie.mkv", 4242)
	got, err := parseTorrentMeta(raw)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if got.InfoHash != want {
		t.Errorf("infohash = %s, want %s", got.InfoHash, want)
	}
	if got.Name != "movie.mkv" {
		t.Errorf("name = %q", got.Name)
	}
	if got.TotalSize != 4242 {
		t.Errorf("size = %d", got.TotalSize)
	}
}

// bs/bi/bl build bencode so a fixture can never lie about a string length.
func bs(v string) string { return itoa(len(v)) + ":" + v }
func bi(n int) string    { return "i" + itoa(n) + "e" }
func bl(items ...string) string {
	return "l" + strings.Join(items, "") + "e"
}

func TestParseResume(t *testing.T) {
	raw := []byte("d" +
		bs("added-date") + bi(1700000000) +
		bs("destination") + bs("/data/movies") +
		bs("done-date") + bi(1700000900) +
		bs("downloaded") + bi(500) +
		bs("incomplete-dir") + bs("/data/inc") +
		bs("labels") + bl(bs("french"), bs("kept")) +
		bs("name") + bs("movie.mkv") +
		bs("paused") + bi(1) +
		bs("seeding-time-seconds") + bi(3600) +
		bs("uploaded") + bi(999) +
		"e")
	ri, err := parseResume(raw)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if !ri.Paused {
		t.Error("paused should be true")
	}
	if ri.Destination != "/data/movies" {
		t.Errorf("destination = %q", ri.Destination)
	}
	if ri.IncompleteDir != "/data/inc" {
		t.Errorf("incomplete = %q", ri.IncompleteDir)
	}
	if ri.Uploaded != 999 || ri.Downloaded != 500 {
		t.Errorf("stats = %d/%d", ri.Uploaded, ri.Downloaded)
	}
	if ri.AddedDate != 1700000000 || ri.DoneDate != 1700000900 {
		t.Errorf("dates = %d / %d", ri.AddedDate, ri.DoneDate)
	}
	if ri.SeedingTime != 3600 {
		t.Errorf("seeding time = %d", ri.SeedingTime)
	}
	if len(ri.Labels) != 2 || ri.Labels[0] != "french" || ri.Labels[1] != "kept" {
		t.Errorf("labels = %v", ri.Labels)
	}
}

// The scan must pair by parsed infohash, never by filename: the naming scheme
// changed between Transmission versions, so both spellings must work, and a
// deliberately misleading filename must not fool it.
func TestScanTransmissionDir(t *testing.T) {
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "torrents"), 0o755)
	os.MkdirAll(filepath.Join(root, "resume"), 0o755)

	rawA, hashA := buildTorrent(t, "alpha.mkv", 100)
	// Filename says a totally different hash on purpose.
	nameA := "alpha.deadbeefdeadbeef.torrent"
	os.WriteFile(filepath.Join(root, "torrents", nameA), rawA, 0o644)
	os.WriteFile(filepath.Join(root, "resume", "alpha.deadbeefdeadbeef.resume"),
		[]byte("d"+bs("destination")+bs("/data/a")+bs("paused")+bi(1)+"e"), 0o644)

	rawB, hashB := buildTorrent(t, "beta.mkv", 200)
	os.WriteFile(filepath.Join(root, "torrents", hashB+".torrent"), rawB, 0o644)
	os.WriteFile(filepath.Join(root, "resume", hashB+".resume"),
		[]byte("d"+bs("destination")+bs("/data/b")+bs("paused")+bi(0)+"e"), 0o644)

	// A file that is not a torrent at all must be reported, not fatal.
	os.WriteFile(filepath.Join(root, "torrents", "junk.torrent"), []byte("nope"), 0o644)

	got, problems, err := scanTransmissionDir(root)
	if err != nil {
		t.Fatalf("scan: %v", err)
	}
	if len(got) != 2 {
		t.Fatalf("got %d torrents, want 2", len(got))
	}
	if len(problems) != 1 {
		t.Errorf("problems = %v, want the junk file only", problems)
	}
	byHash := map[string]transmissionTorrent{}
	for _, g := range got {
		byHash[g.Meta.InfoHash] = g
	}
	a, ok := byHash[hashA]
	if !ok {
		t.Fatalf("alpha not found by its real hash (filename lied)")
	}
	if !a.HasResume || a.Resume.Destination != "/data/a" || !a.Resume.Paused {
		t.Errorf("alpha resume = %+v", a.Resume)
	}
	b := byHash[hashB]
	if !b.HasResume || b.Resume.Paused {
		t.Errorf("beta resume = %+v", b.Resume)
	}
}

func TestSavePathForIncomplete(t *testing.T) {
	tt := transmissionTorrent{Resume: resumeInfo{
		Destination: "/data/done", IncompleteDir: "/data/inc"}}
	if p := tt.savePathFor(false); p != "/data/inc" {
		t.Errorf("incomplete should adopt from the incomplete dir, got %q", p)
	}
	if p := tt.savePathFor(true); p != "/data/done" {
		t.Errorf("complete should adopt from the destination, got %q", p)
	}
}
