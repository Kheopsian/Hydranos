package engine

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// bencodeSingle/bencodeMulti build the two .torrent shapes by hand: the point of
// the check under test is the layout on disk, so the metainfo has to be real
// bencode rather than a fixture nobody can read.
func bencodeSingle(name string, length int64) []byte {
	return []byte("d4:infod6:lengthi" + itoa(length) + "e4:name" + itoa(int64(len(name))) + ":" + name + "ee")
}

func bencodeMulti(name string, files map[string]int64, order []string) []byte {
	var b strings.Builder
	b.WriteString("d4:infod5:filesl")
	for _, rel := range order {
		parts := strings.Split(rel, "/")
		b.WriteString("d6:lengthi" + itoa(files[rel]) + "e4:pathl")
		for _, p := range parts {
			b.WriteString(itoa(int64(len(p))) + ":" + p)
		}
		b.WriteString("ee")
	}
	b.WriteString("e4:name" + itoa(int64(len(name))) + ":" + name + "ee")
	return []byte(b.String())
}

func itoa(n int64) string {
	if n == 0 {
		return "0"
	}
	var digits []byte
	for n > 0 {
		digits = append([]byte{byte('0' + n%10)}, digits...)
		n /= 10
	}
	return string(digits)
}

func write(t *testing.T, path string, size int) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, make([]byte, size), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestTorrentFileListSingle(t *testing.T) {
	name, multi, files, err := torrentFileList(bencodeSingle("film.mkv", 42))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if name != "film.mkv" || multi || len(files) != 1 || files[0].Path != "film.mkv" || files[0].Length != 42 {
		t.Fatalf("got name=%q multi=%v files=%+v", name, multi, files)
	}
}

func TestTorrentFileListMulti(t *testing.T) {
	raw := bencodeMulti("Release", map[string]int64{"a.mkv": 10, "subs/en.srt": 3}, []string{"a.mkv", "subs/en.srt"})
	name, multi, files, err := torrentFileList(raw)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if name != "Release" || !multi || len(files) != 2 {
		t.Fatalf("got name=%q multi=%v files=%+v", name, multi, files)
	}
	if files[1].Path != filepath.Join("subs", "en.srt") || files[1].Length != 3 {
		t.Fatalf("second file wrong: %+v", files[1])
	}
}

// The whole point of skip-recheck: it must refuse rather than register a
// torrent that claims to be complete over data that is not there.
func TestVerifyPayloadPresentSingleFile(t *testing.T) {
	dir := t.TempDir()
	raw := bencodeSingle("film.mkv", 12)

	if err := verifyPayloadPresent(raw, dir); err == nil {
		t.Fatal("missing payload accepted")
	}

	write(t, filepath.Join(dir, "film.mkv"), 12)
	if err := verifyPayloadPresent(raw, dir); err != nil {
		t.Fatalf("present payload refused: %v", err)
	}

	// Right name, wrong size: a truncated or half-copied file is exactly the
	// case a hash check would have caught.
	write(t, filepath.Join(dir, "film.mkv"), 11)
	err := verifyPayloadPresent(raw, dir)
	if err == nil || !strings.Contains(err.Error(), "wrong size") {
		t.Fatalf("wrong-size payload: %v", err)
	}
}

// A multi-file torrent lives under <engine save_path>/<info.name>/, which is
// where Typhon writes it. Pointing the check one level off has to fail.
func TestVerifyPayloadPresentMultiFile(t *testing.T) {
	dir := t.TempDir()
	raw := bencodeMulti("Release", map[string]int64{"a.mkv": 10, "subs/en.srt": 3}, []string{"a.mkv", "subs/en.srt"})

	write(t, filepath.Join(dir, "Release", "a.mkv"), 10)
	if err := verifyPayloadPresent(raw, dir); err == nil {
		t.Fatal("partial payload accepted")
	}
	write(t, filepath.Join(dir, "Release", "subs", "en.srt"), 3)
	if err := verifyPayloadPresent(raw, dir); err != nil {
		t.Fatalf("complete payload refused: %v", err)
	}
	// Same data, save path taken one level too deep.
	if err := verifyPayloadPresent(raw, filepath.Join(dir, "Release")); err == nil {
		t.Fatal("wrong save path accepted")
	}
}

func TestAddOptionsWrapFolder(t *testing.T) {
	yes, no := true, false
	if !(AddOptions{}).WrapFolder(true) || (AddOptions{}).WrapFolder(false) {
		t.Fatal("no override should follow the daemon default")
	}
	if !(AddOptions{CreateFolder: &yes}).WrapFolder(false) {
		t.Fatal("explicit yes ignored")
	}
	if (AddOptions{CreateFolder: &no}).WrapFolder(true) {
		t.Fatal("explicit no ignored")
	}
}
