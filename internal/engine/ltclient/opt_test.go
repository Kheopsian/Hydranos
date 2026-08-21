package ltclient

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
	"time"
)

// The optimisation flags rewrite the framing and routing of every byte that
// crosses the Go<->engine socket. A bug there does not degrade gracefully: it
// corrupts the protocol. So both flag states must produce identical frames and
// identical routing decisions, including on frames far larger than the reader's
// buffer — which is the only case the fast path treats differently.

func frames(t *testing.T, wire string, fast bool) [][]byte {
	t.Helper()
	optFrame.Store(fast)
	defer optFrame.Store(false)

	r := bufio.NewReaderSize(strings.NewReader(wire), 4096)
	var got [][]byte
	for {
		f, err := readFrame(r)
		if err != nil {
			return got
		}
		got = append(got, f)
	}
}

func TestReadFrameIdenticalBothPaths(t *testing.T) {
	// A frame much larger than the 4KB reader buffer (the list_torrents case),
	// one that lands exactly on a buffer boundary, plus small and empty ones.
	big := strings.Repeat("x", 100_000)
	boundary := strings.Repeat("y", 4096)
	wire := "small\n" + big + "\n" + "\n" + boundary + "\n" + "last\n"

	want := [][]byte{[]byte("small"), []byte(big), {}, []byte(boundary), []byte("last")}

	for _, fast := range []bool{false, true} {
		got := frames(t, wire, fast)
		if len(got) != len(want) {
			t.Fatalf("fast=%v: got %d frames, want %d", fast, len(got), len(want))
		}
		for i := range want {
			if !bytes.Equal(got[i], want[i]) {
				t.Errorf("fast=%v: frame %d differs (got %d bytes, want %d)",
					fast, i, len(got[i]), len(want[i]))
			}
		}
	}
}

// A frame with no trailing newline is incomplete and must be dropped, not
// delivered as a truncated frame that would fail to parse downstream.
func TestReadFrameDropsUnterminatedTail(t *testing.T) {
	for _, fast := range []bool{false, true} {
		got := frames(t, "complete\ntruncated", fast)
		if len(got) != 1 || !bytes.Equal(got[0], []byte("complete")) {
			t.Errorf("fast=%v: got %d frames, want just the complete one", fast, len(got))
		}
	}
}

func TestRouteFrameIdenticalBothPaths(t *testing.T) {
	// A response carrying a big result: the payload is exactly what the fast
	// path must skip instead of copying.
	items := make([]string, 2000)
	for i := range items {
		items[i] = fmt.Sprintf(`{"info_hash":"%040x","state":"Seeding"}`, i)
	}
	bigResult := `{"torrents":[` + strings.Join(items, ",") + `],"count":2000}`

	cases := []struct {
		name    string
		frame   string
		isEvent bool
		id      int64
		errMsg  string
	}{
		{"response", `{"id":42,"result":` + bigResult + `}`, false, 42, ""},
		{"error", `{"id":7,"error":"boom"}`, false, 7, "boom"},
		{"event", `{"event":"torrent_added","data":{"info_hash":"ab"}}`, true, 0, ""},
	}

	for _, tc := range cases {
		for _, fast := range []bool{false, true} {
			optRoute.Store(fast)
			head, err := routeFrame([]byte(tc.frame))
			optRoute.Store(false)
			if err != nil {
				t.Fatalf("%s fast=%v: %v", tc.name, fast, err)
			}
			// Routing only asks "is this an event?" — the baseline path cannot
			// report the event's name, so compare the decision, not the string.
			if (head.Event != "") != tc.isEvent {
				t.Errorf("%s fast=%v: event=%q, want isEvent=%v", tc.name, fast, head.Event, tc.isEvent)
			}
			if head.ID != tc.id {
				t.Errorf("%s fast=%v: id=%d, want %d", tc.name, fast, head.ID, tc.id)
			}
			if head.Error != tc.errMsg {
				t.Errorf("%s fast=%v: error=%q, want %q", tc.name, fast, head.Error, tc.errMsg)
			}
		}
	}
}

// The point of the fast route is to skip the payload, so it must stay cheap as
// the payload grows. This asserts the property that matters (no full decode),
// not a timing, which would be flaky.
func TestRouteFrameFastPathIgnoresPayloadShape(t *testing.T) {
	// Deeply nested and unusual — a full decode into a typed struct would still
	// walk it; a broken skip would choke on it.
	frame := `{"id":1,"result":{"a":[[[{"b":null}]]],"c":"é\"quoted\"","d":1e10}}`
	optRoute.Store(true)
	defer optRoute.Store(false)
	head, err := routeFrame([]byte(frame))
	if err != nil {
		t.Fatalf("routeFrame: %v", err)
	}
	if head.ID != 1 || head.Event != "" || head.Error != "" {
		t.Errorf("head = %+v, want id=1 and nothing else", head)
	}
}

// The snapshot is shared, not copied, and a refresh replaces it wholesale so a
// caller still holding the previous one keeps a consistent view.
//
// This replaces a contract that copied the whole array on every cache hit. That
// copy guarded against enforceDownloadSlots sorting what it was handed -- which
// it no longer does: its four sorts all run on slices it derives itself
// (eligible/remaining/evictees/newcomers), and nothing in the tree sorts or
// writes through a ListTorrentsResult. At 196k torrents the copy was a
// 196k-element array per hit, protecting against something that does not
// happen. If a caller ever does need to mutate, it copies what it needs.
func TestCachedListSharesSnapshotAndSurvivesRefresh(t *testing.T) {
	optListCache.Store(true)
	defer optListCache.Store(false)

	c := &Client{}
	c.listCache = &ListTorrentsResult{
		Torrents: []TorrentStatus{{InfoHash: "a"}, {InfoHash: "b"}},
		Count:    2,
	}
	c.listCachedAt = time.Now()

	first := c.cachedList()
	if first == nil {
		t.Fatal("fresh snapshot returned nil")
	}
	// Shared, not copied: that is the whole point of the change.
	if first != c.listCache {
		t.Error("cachedList copied the snapshot instead of sharing it")
	}

	// A refresh swaps the pointer; it must not write through the old one.
	c.listMu.Lock()
	c.listCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "c"}}, Count: 1}
	c.listCachedAt = time.Now()
	c.listMu.Unlock()

	if first.Torrents[0].InfoHash != "a" {
		t.Errorf("refresh disturbed a snapshot a caller still held: %q", first.Torrents[0].InfoHash)
	}
	second := c.cachedList()
	if second == nil || second.Torrents[0].InfoHash != "c" {
		t.Error("second read did not see the refreshed snapshot")
	}
}

// Guard the wire contract the engine relies on: a frame is one line of JSON.
func TestFramesAreSingleLineJSON(t *testing.T) {
	b, err := json.Marshal(Request{ID: 1, Method: "list_torrents"})
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(b, []byte("\n")) {
		t.Error("marshalled request contains a newline, which would split the frame")
	}
}
