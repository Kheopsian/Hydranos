package engine

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// stubResumeClient serves a fixed listing plus per-torrent trackers, which is
// all resumeAnnounceTargets reads.
type stubResumeClient struct {
	stubSlotClient
	trackers map[string][]ltclient.TrackerInfo
}

func (c *stubResumeClient) GetTrackers(infoHash string) ([]ltclient.TrackerInfo, error) {
	return c.trackers[infoHash], nil
}

func newResumeEngine(t *testing.T, rows []ltclient.TorrentStatus, trackers map[string][]ltclient.TrackerInfo) *RaceEngine {
	t.Helper()
	c := &stubResumeClient{
		stubSlotClient: stubSlotClient{torrents: map[string]*ltclient.TorrentStatus{}},
		trackers:       trackers,
	}
	for i := range rows {
		r := rows[i]
		c.torrents[r.InfoHash] = &r
		c.order = append(c.order, r.InfoHash)
	}
	e := NewRaceEngine(&config.SessionConfig{ListenPort: 16171}, nil, nil, t.TempDir())
	e.SetClient(c)
	return e
}

func trackerRow(urls ...string) []ltclient.TrackerInfo {
	out := make([]ltclient.TrackerInfo, 0, len(urls))
	for _, u := range urls {
		out = append(out, ltclient.TrackerInfo{URL: u})
	}
	return out
}

// The bug this pins down: raceAnnounceLoop only ever starts from the add path
// and stops at completion, and the keepalive only walks finished torrents. A
// torrent that was mid-download when the process went down therefore came back
// announcing to nobody -- no announce, no peers, no download, forever.
func TestResumeAnnounceTargetsArmsUnfinishedDownloads(t *testing.T) {
	rows := []ltclient.TorrentStatus{
		{InfoHash: "aa", State: "downloading", TotalSize: 100},
		{InfoHash: "bb", State: "seeding", IsFinished: true, TotalSize: 200},
		{InfoHash: "cc", State: "stopped", IsPaused: true, TotalSize: 300},
		{InfoHash: "dd", State: "error", TotalSize: 400},
	}
	trk := map[string][]ltclient.TrackerInfo{
		"aa": trackerRow("http://a.invalid/announce"),
		"bb": trackerRow("http://b.invalid/announce"),
		"cc": trackerRow("http://c.invalid/announce"),
		"dd": trackerRow("http://d.invalid/announce"),
	}

	got := newResumeEngine(t, rows, trk).resumeAnnounceTargets()
	if len(got) != 1 {
		t.Fatalf("armed %d torrents, want only the unfinished one: %+v", len(got), got)
	}
	if got[0].infoHash != "aa" {
		t.Errorf("armed %q, want the downloading torrent", got[0].infoHash)
	}
	if got[0].totalSize != 100 {
		t.Errorf("totalSize = %d, want 100 — the loop announces `left` from it", got[0].totalSize)
	}
}

// A torrent the engine holds no reachable tracker for would spin a loop that
// can never announce.
func TestResumeAnnounceTargetsSkipsTorrentsWithNoUsableTracker(t *testing.T) {
	rows := []ltclient.TorrentStatus{
		{InfoHash: "aa", State: "downloading", TotalSize: 100},
		{InfoHash: "bb", State: "downloading", TotalSize: 100},
	}
	trk := map[string][]ltclient.TrackerInfo{
		"aa": nil,
		"bb": trackerRow("tcp://b.invalid:6970"),
	}

	if got := newResumeEngine(t, rows, trk).resumeAnnounceTargets(); len(got) != 0 {
		t.Fatalf("armed %+v, want nothing", got)
	}
}

// Unsupported schemes are dropped, and a torrent keeps the ones we can speak.
func TestSupportedTrackerURLsKeepsOnlyWhatWeCanSpeak(t *testing.T) {
	rows := []ltclient.TorrentStatus{{InfoHash: "aa", State: "downloading"}}
	trk := map[string][]ltclient.TrackerInfo{
		"aa": trackerRow(
			"http://t.invalid:6969/announce",
			"udp://t.invalid:6969/announce",
			"tcp://t.invalid:6970",
			"wss://t.invalid/announce",
		),
	}

	got := newResumeEngine(t, rows, trk).supportedTrackerURLs("aa")
	want := []string{"http://t.invalid:6969/announce", "udp://t.invalid:6969/announce"}
	if len(got) != len(want) {
		t.Fatalf("got %q, want %q", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("got %q, want %q", got, want)
		}
	}
}
