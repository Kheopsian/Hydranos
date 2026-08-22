package api

import (
	"encoding/json"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

func newTestRecorder(startTs float64) *raceRecorder {
	return &raceRecorder{startTs: startTs, seen: map[string]bool{}, done: map[string]bool{}}
}

// A torrent that was already on the agent gets no "added": the timeline is
// drawn from that marker, and dating it at boot would shift the whole graph.
func TestLifecycleEventSkipsPreexistingTorrents(t *testing.T) {
	r := newTestRecorder(1000)
	old := ltclient.TorrentStatus{InfoHash: "aa", AddedTime: 900, Progress: 0.5}
	if _, ok := r.lifecycleEvent(old, "", 1005); ok {
		t.Fatal("logged an added event for a torrent older than this recorder")
	}
	fresh := ltclient.TorrentStatus{InfoHash: "bb", AddedTime: 1002, Progress: 0}
	ev, ok := r.lifecycleEvent(fresh, "movies", 1005)
	if !ok || ev.Event != "added" {
		t.Fatalf("new torrent produced %+v (ok=%v), want an added event", ev, ok)
	}
	if ev.Ts != 1002 || ev.Category != "movies" {
		t.Fatalf("added event carries ts=%v category=%q, want 1002/movies", ev.Ts, ev.Category)
	}
}

func TestLifecycleEventFiresOnceOnCompletion(t *testing.T) {
	r := newTestRecorder(1000)
	dl := ltclient.TorrentStatus{InfoHash: "aa", AddedTime: 1001, Progress: 0.4}
	r.lifecycleEvent(dl, "", 1010) // added

	done := ltclient.TorrentStatus{InfoHash: "aa", AddedTime: 1001, Progress: 1.0}
	ev, ok := r.lifecycleEvent(done, "", 1060)
	if !ok || ev.Event != "completed" {
		t.Fatalf("completion produced %+v (ok=%v), want a completed event", ev, ok)
	}
	if ev.DownloadTime != 59 {
		t.Errorf("download_time = %v, want 59", ev.DownloadTime)
	}
	if _, ok := r.lifecycleEvent(done, "", 1065); ok {
		t.Error("completion logged twice: every sample would write another event")
	}
}

// A torrent seen for the first time already complete never downloaded here, so
// it must not produce a completion the panel would date to now.
func TestLifecycleEventIgnoresAlreadyCompleteOnFirstSight(t *testing.T) {
	r := newTestRecorder(1000)
	seeded := ltclient.TorrentStatus{InfoHash: "aa", AddedTime: 500, Progress: 1.0}
	if _, ok := r.lifecycleEvent(seeded, "", 1005); ok {
		t.Fatal("first sighting of a complete torrent produced an event")
	}
	if _, ok := r.lifecycleEvent(seeded, "", 1010); ok {
		t.Fatal("second sighting produced a completion for a download nobody watched")
	}
}

func TestSnapshotSkipsIdleSeeds(t *testing.T) {
	r := newTestRecorder(1000)
	idle := ltclient.TorrentStatus{InfoHash: "aa", Progress: 1.0}
	if _, ok := r.snapshot(nil, idle, 1005); ok {
		t.Fatal("an idle seed was sampled: it would write a row every 5s forever")
	}
	busy := ltclient.TorrentStatus{InfoHash: "bb", Progress: 1.0, UploadRate: 10}
	snap, ok := r.snapshot(nil, busy, 1005)
	if !ok {
		t.Fatal("a seeding torrent with upload was not sampled")
	}
	if snap.PeersJSON != "[]" {
		t.Errorf("peers_json = %q, want [] when there is nothing to fetch", snap.PeersJSON)
	}
}

func TestRacePeersJSONKeepsFastAndNearCompletePeers(t *testing.T) {
	var peers []ltclient.PeerInfo
	for i := 0; i < 12; i++ {
		peers = append(peers, ltclient.PeerInfo{IP: "10.0.0.1", Port: i, DLRate: int64(100 - i), Progress: 0.1})
	}
	peers = append(peers, ltclient.PeerInfo{IP: "10.0.0.99", Port: 999, DLRate: 0, Progress: 1.0, Client: "seed"})

	var out []map[string]interface{}
	if err := json.Unmarshal([]byte(racePeersJSON(peers)), &out); err != nil {
		t.Fatalf("peers json does not parse: %v", err)
	}
	if len(out) != raceTimelinePeerRows+1 {
		t.Fatalf("kept %d peers, want the %d fastest plus the seed", len(out), raceTimelinePeerRows)
	}
	if out[len(out)-1]["client"] != "seed" {
		t.Error("the complete-but-idle seed was dropped; it is half the answer to who fed the download")
	}
	if out[0]["port"] != "0" {
		t.Errorf("port = %v, want the string \"0\" the panel expects", out[0]["port"])
	}
}

// One agent timing out must not evict the torrents it holds: they would be
// sighted as new on the next tick and get a second "added" in a timeline whose
// whole job is to say when things happened.
func TestForgetDepartedWaitsForACompletePass(t *testing.T) {
	r := newTestRecorder(1000)
	r.seen["aa"], r.seen["bb"] = true, true
	r.done["aa"] = true

	// bb lives on the agent that did not answer, so it is missing from live.
	r.forgetDeparted(map[string]bool{"aa": true}, false)
	if !r.seen["bb"] {
		t.Fatal("a torrent on an agent that timed out was forgotten; the next tick logs it as added again")
	}

	// A pass that reached every engine is authoritative: bb really is gone.
	r.forgetDeparted(map[string]bool{"aa": true}, true)
	if r.seen["bb"] {
		t.Fatal("a departed torrent was kept: these maps would grow for the life of the process")
	}
	if !r.seen["aa"] || !r.done["aa"] {
		t.Fatal("a live torrent lost its lifecycle state")
	}
}
