package btmeta

import (
	"bytes"
	"crypto/sha1"
	"encoding/hex"
	"testing"
)

// A minimal but realistic torrent: keys out of the order we would emit them,
// an info dict with its own nesting, and a comment key that must survive.
const sample = "d" +
	"8:announce22:http://old.invalid/ann" +
	"13:announce-listll22:http://old.invalid/annel22:http://bak.invalid/annee" +
	"7:comment5:hello" +
	"4:infod6:lengthi1234e4:name4:file12:piece lengthi16384e6:pieces0:e" +
	"e"

func infoHash(t *testing.T, raw []byte) string {
	t.Helper()
	span, err := InfoSpan(raw)
	if err != nil {
		t.Fatalf("InfoSpan: %v", err)
	}
	sum := sha1.Sum(span)
	return hex.EncodeToString(sum[:])
}

// The one thing that must never happen: editing trackers must not change what
// torrent this is. Everything else in this package is in service of this test.
func TestEditingTrackersKeepsTheInfoHash(t *testing.T) {
	raw := []byte(sample)
	before := infoHash(t, raw)
	beforeSpan, _ := InfoSpan(raw)

	out, err := SetAnnounce(raw, [][]string{{"http://new.invalid/ann"}, {"udp://fallback.invalid:6969"}})
	if err != nil {
		t.Fatalf("SetAnnounce: %v", err)
	}
	if got := infoHash(t, out); got != before {
		t.Fatalf("infohash changed: %s -> %s", before, got)
	}
	afterSpan, _ := InfoSpan(out)
	if !bytes.Equal(beforeSpan, afterSpan) {
		t.Fatalf("info dict bytes changed:\n before %q\n after  %q", beforeSpan, afterSpan)
	}
}

func TestRoundTripOfTiers(t *testing.T) {
	want := [][]string{
		{"http://a.invalid/ann", "http://a2.invalid/ann"},
		{"udp://b.invalid:6969"},
	}
	out, err := SetAnnounce([]byte(sample), want)
	if err != nil {
		t.Fatalf("SetAnnounce: %v", err)
	}
	got, err := Announce(out)
	if err != nil {
		t.Fatalf("Announce: %v", err)
	}
	if len(got) != len(want) {
		t.Fatalf("tiers = %v, want %v", got, want)
	}
	for i := range want {
		if len(got[i]) != len(want[i]) {
			t.Fatalf("tier %d = %v, want %v", i, got[i], want[i])
		}
		for j := range want[i] {
			if got[i][j] != want[i][j] {
				t.Fatalf("tier %d url %d = %q, want %q", i, j, got[i][j], want[i][j])
			}
		}
	}
	// The fallback key has to stay in step for clients that predate BEP 12.
	if !bytes.Contains(out, []byte("8:announce20:http://a.invalid/ann")) {
		t.Fatalf("announce key does not carry the first url: %q", out)
	}
}

// Removing every tracker has to remove both keys. An empty announce-list reads
// as "one tracker with an empty URL" in some clients.
func TestClearingTrackersRemovesBothKeys(t *testing.T) {
	out, err := SetAnnounce([]byte(sample), nil)
	if err != nil {
		t.Fatalf("SetAnnounce: %v", err)
	}
	if bytes.Contains(out, []byte("8:announce")) || bytes.Contains(out, []byte("13:announce-list")) {
		t.Fatalf("announce keys survived: %q", out)
	}
	if infoHash(t, out) != infoHash(t, []byte(sample)) {
		t.Fatal("clearing trackers changed the infohash")
	}
	tiers, err := Announce(out)
	if err != nil || len(tiers) != 0 {
		t.Fatalf("Announce after clear = %v, %v", tiers, err)
	}
}

// Keys we do not understand must come through untouched, in valid order.
func TestUnknownKeysSurvive(t *testing.T) {
	out, err := SetAnnounce([]byte(sample), [][]string{{"http://new.invalid/ann"}})
	if err != nil {
		t.Fatalf("SetAnnounce: %v", err)
	}
	if !bytes.Contains(out, []byte("7:comment5:hello")) {
		t.Fatalf("comment key lost: %q", out)
	}
	entries, err := topLevel(out)
	if err != nil {
		t.Fatalf("re-parse: %v", err)
	}
	for i := 1; i < len(entries); i++ {
		if entries[i-1].key >= entries[i].key {
			t.Fatalf("keys not sorted: %q then %q", entries[i-1].key, entries[i].key)
		}
	}
}

func TestRefusesGarbage(t *testing.T) {
	for _, bad := range []string{"", "x", "d", "d8:announce"} {
		if _, err := SetAnnounce([]byte(bad), nil); err == nil {
			t.Fatalf("accepted %q", bad)
		}
	}
}

// The test above is only worth something if the check can fail. Touching the
// info dict has to move the hash: if it did not, "the infohash is unchanged"
// would be a statement about nothing.
func TestInfoHashCheckHasTeeth(t *testing.T) {
	raw := []byte(sample)
	tampered := bytes.Replace(raw, []byte("4:name4:file"), []byte("4:name4:FILE"), 1)
	if bytes.Equal(raw, tampered) {
		t.Fatal("tampering did not change the input")
	}
	if infoHash(t, raw) == infoHash(t, tampered) {
		t.Fatal("the infohash did not move when the info dict changed")
	}
}
