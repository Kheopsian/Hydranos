package agent

import (
	"encoding/json"
	"fmt"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Phase 0 of the "1 agent = 1 engine" study: what does routing a LOCAL engine's
// list through the agent wire actually cost?
//
// The wire encodes every reply with json.Marshal (server.go reply()), so making
// the monolith's primaries into agents puts a full JSON round-trip of the whole
// listing on the hottest path in the product. This measures that round-trip
// against the in-process path, which hands back the slice and copies nothing.
//
// Deterministic and load-independent on purpose: no daemon, no socket, no
// competing traffic. It measures the marginal cost of the encoding, which is the
// part that scales with the torrent count.
func synthList(n int) *ltclient.ListTorrentsResult {
	ts := make([]ltclient.TorrentStatus, n)
	for i := range ts {
		ts[i] = ltclient.TorrentStatus{
			InfoHash:       fmt.Sprintf("%040x", i),
			Name:           "Some.Release.Name.2026.1080p.BluRay.x265-GROUP",
			State:          "seeding",
			Progress:       1,
			TotalSize:      12884901888,
			TotalDone:      12884901888,
			TotalUpload:    3221225472,
			SavePath:       "/data/movies/Some.Release.Name.2026",
			CurrentTracker: "https://tracker.example.net/announce/passkey",
			TrackerHost:    "tracker.example.net",
			IsSeeding:      true,
			IsFinished:     true,
			IsAnnounced:    true,
			NumPieces:      6144,
			PieceLength:    2097152,
			SeedingTime:    864000,
			ActiveTime:     864000,
			AddedTime:      1750000000,
			CompletedTime:  1750000001,
		}
	}
	return &ltclient.ListTorrentsResult{Torrents: ts, Count: n}
}

const prodScale = 196000

// In-process: what the monolith does today. The API holds the engine and reads
// the result directly.
func BenchmarkListInProcess(b *testing.B) {
	res := synthList(prodScale)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		out := res
		if out.Count != prodScale {
			b.Fatal("bad list")
		}
	}
}

// Through the agent wire: marshal on the agent side, unmarshal on the caller
// side. Exactly what a local engine promoted to an agent would pay per list.
func BenchmarkListViaAgentWire(b *testing.B) {
	res := synthList(prodScale)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		blob, err := json.Marshal(res)
		if err != nil {
			b.Fatal(err)
		}
		var back ltclient.ListTorrentsResult
		if err := json.Unmarshal(blob, &back); err != nil {
			b.Fatal(err)
		}
		if back.Count != prodScale {
			b.Fatal("bad list")
		}
	}
}

// The marshal half alone, to see which side of the hop dominates.
func BenchmarkListWireMarshalOnly(b *testing.B) {
	res := synthList(prodScale)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if _, err := json.Marshal(res); err != nil {
			b.Fatal(err)
		}
	}
}
