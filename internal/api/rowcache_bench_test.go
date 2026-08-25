package api

import (
	"fmt"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// What would it cost to stop special-casing this node and let every total come
// from the agent-row cache, as the design intends?
//
// The row cache holds one map[string]interface{} per torrent and is rebuilt on
// each refresh. Today this node's torrents never enter it: the local path feeds
// the totals directly from cached aggregates. Routing them through the cache is
// architecturally cleaner -- exactly one producer of the numbers -- so the
// question is what the cleanliness costs at production scale.
func benchRows(b *testing.B, n int) {
	ts := make([]ltclient.TorrentStatus, n)
	for i := range ts {
		ts[i] = ltclient.TorrentStatus{
			InfoHash: fmt.Sprintf("%040x", i),
			Name:     "Some.Release.Name.2026.1080p.BluRay.x265-GROUP",
			State:    "seeding", Progress: 1,
			TotalSize: 12884901888, TotalDone: 12884901888, TotalUpload: 3221225472,
			SavePath: "/data/movies/x", TrackerHost: "tracker.example.net",
			IsSeeding: true, IsFinished: true,
		}
	}
	cats := map[string]string{}
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		rows := make([]map[string]interface{}, 0, len(ts))
		for _, t := range ts {
			rows = append(rows, ltStatusToRow(t, "local-hoard", cats))
		}
		if len(rows) != n {
			b.Fatal("short")
		}
	}
}

// Production scale as of 2026-08-25.
func BenchmarkRowCacheAtProdScale(b *testing.B) { benchRows(b, 198355) }

// The scale the EPYC box is being bought for.
func BenchmarkRowCacheAtOneMillion(b *testing.B) { benchRows(b, 1000000) }
