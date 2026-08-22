package api

import (
	"context"
	"encoding/json"
	"log/slog"
	"sort"
	"strconv"
	"time"

	"github.com/Kheopsian/hydra/internal/bench"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

const (
	// Same cadence the monolith samples its own race engine at, so a torrent's
	// timeline has the same resolution wherever it ran.
	raceTimelineInterval = 5 * time.Second
	raceTimelinePurge    = time.Hour
	raceTimelineListTO   = 4 * time.Second
	// Peers are read per downloading torrent, over the network here. Ten is
	// what the panel shows.
	raceTimelinePeerRows = 10
)

// StartRaceTimelineRecorder samples the agents' race engines into the bench DB.
//
// The monolith fills the timeline from its own engine's events and a local
// 5s sampler. A controller node has no engine, so every torrent on an agent
// answered /api/race/timeline with nothing at all and the panel read "No
// timeline data" for its whole life. Sampling from here is what the front is
// already positioned to do: it is the node that talks to every agent.
func (s *Server) StartRaceTimelineRecorder(ctx context.Context) {
	if s.benchDB == nil {
		return
	}
	rec := &raceRecorder{srv: s, startTs: float64(time.Now().Unix()), seen: map[string]bool{}, done: map[string]bool{}}
	go func() {
		t := time.NewTicker(raceTimelineInterval)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				rec.tick()
			}
		}
	}()
	go func() {
		t := time.NewTicker(raceTimelinePurge)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				s.benchDB.PurgeOld()
			}
		}
	}()
	slog.Info("race timeline recorder started (agents are sampled from this node)")
}

// raceRecorder remembers which torrents it has already logged a lifecycle event
// for. Events are derived by watching the listing change, since the engines
// emitting them are on other nodes.
type raceRecorder struct {
	srv     *Server
	startTs float64
	seen    map[string]bool
	done    map[string]bool
}

func (r *raceRecorder) tick() {
	now := float64(time.Now().Unix())
	var snapshots []bench.RaceSnapshot
	live := map[string]bool{}
	var agg raceAgg
	engines, answered := 0, 0

	for _, ra := range r.srv.agentsSnapshot() {
		for _, e := range ra.byRole("race") {
			if e.client == nil {
				continue
			}
			engines++
			lst, err := e.client.ListTorrentsTimeout(raceTimelineListTO)
			if err != nil || lst == nil {
				continue
			}
			answered++
			cats, _ := e.client.TorrentCategories(e.id)
			for _, t := range lst.Torrents {
				live[t.InfoHash] = true
				agg.add(t)
				if ev, ok := r.lifecycleEvent(t, cats[t.InfoHash], now); ok {
					r.srv.benchDB.InsertRaceEvent(ev)
				}
				if snap, ok := r.snapshot(e.client, t, now); ok {
					snapshots = append(snapshots, snap)
				}
			}
		}
	}

	if len(snapshots) > 0 {
		r.srv.benchDB.InsertRaceSnapshots(snapshots)
	}
	// This pass already listed every race engine, so the overview totals come
	// free with it -- see refreshAgentRaceStats, which stands down while this
	// is running.
	r.srv.publishRaceAgg(agg, engines, answered)
	r.forgetDeparted(live, engines == answered)
}

// forgetDeparted drops the torrents that are no longer on any agent. A torrent
// that left is not coming back under the same lifecycle, and keeping it would
// grow these maps for the life of the process.
//
// Only when the pass saw every engine. An agent that timed out contributed
// nothing to live, so its torrents all look departed -- forget them and the
// next tick sights them as new, writing a second "added" for a torrent that
// never went anywhere. One 4s timeout would be enough to put a duplicate in a
// timeline that is supposed to be the record of what happened.
func (r *raceRecorder) forgetDeparted(live map[string]bool, complete bool) {
	if !complete {
		return
	}
	for ih := range r.seen {
		if !live[ih] {
			delete(r.seen, ih)
			delete(r.done, ih)
		}
	}
}

// lifecycleEvent returns the added/completed marker this sighting produced, if
// any. A torrent already on an agent when this node started gets no "added":
// its real one happened before anything here was watching, and dating it now
// would put the whole timeline in the wrong place.
func (r *raceRecorder) lifecycleEvent(t ltclient.TorrentStatus, category string, now float64) (bench.RaceEvent, bool) {
	first := !r.seen[t.InfoHash]
	r.seen[t.InfoHash] = true
	if first {
		if t.Progress >= 1.0 {
			r.done[t.InfoHash] = true
		}
		if float64(t.AddedTime) >= r.startTs {
			return raceEvent("added", t, category, float64(t.AddedTime), 0), true
		}
		return bench.RaceEvent{}, false
	}
	if t.Progress >= 1.0 && !r.done[t.InfoHash] {
		r.done[t.InfoHash] = true
		var dlTime float64
		if t.AddedTime > 0 {
			dlTime = now - float64(t.AddedTime)
		}
		return raceEvent("completed", t, category, now, dlTime), true
	}
	return bench.RaceEvent{}, false
}

func raceEvent(kind string, t ltclient.TorrentStatus, category string, ts, dlTime float64) bench.RaceEvent {
	var sinceAdd float64
	if t.AddedTime > 0 {
		sinceAdd = ts - float64(t.AddedTime)
	}
	return bench.RaceEvent{
		Ts: ts, InfoHash: t.InfoHash, Event: kind, Name: t.Name, Size: t.TotalSize,
		DownloadTime: dlTime, UploadTotal: t.TotalUpload, DownloadTotal: t.TotalDownload,
		UploadRate: float64(t.UploadRate), DownloadRate: float64(t.DownloadRate),
		Peers: t.NumPeers, Seeds: t.ListSeeds, SwarmSeeds: t.ListSeeds, SwarmLeechers: t.ListPeers,
		Category: category, TimeSinceAdd: sinceAdd,
	}
}

// snapshot builds one sample, or reports false for a torrent doing nothing --
// an idle seed would otherwise write a row every 5 seconds forever.
func (r *raceRecorder) snapshot(cl *grpcclient.Client, t ltclient.TorrentStatus, now float64) (bench.RaceSnapshot, bool) {
	active := t.DownloadRate > 0 || t.UploadRate > 0 || t.NumPeers > 0 || t.Progress < 1.0
	if !active {
		return bench.RaceSnapshot{}, false
	}
	peersJSON := "[]"
	if t.Progress < 1.0 && t.NumPeers > 0 {
		if peers, err := cl.GetPeers(t.InfoHash); err == nil && len(peers) > 0 {
			peersJSON = racePeersJSON(peers)
		}
	}
	var ratio float64
	if t.TotalDownload > 0 {
		ratio = float64(t.TotalUpload) / float64(t.TotalDownload)
	}
	return bench.RaceSnapshot{
		Ts: now, InfoHash: t.InfoHash, Progress: t.Progress,
		UploadRate: float64(t.UploadRate), DownloadRate: float64(t.DownloadRate),
		TotalUpload: t.TotalUpload, TotalDownload: t.TotalDownload,
		Peers: t.NumPeers, Seeds: t.ListSeeds,
		SwarmSeeds: t.ListSeeds, SwarmLeechers: t.ListPeers,
		Ratio: ratio, PeersJSON: peersJSON,
	}, true
}

// racePeersJSON keeps the fastest peers plus every near-complete one: the panel
// is read to answer "who fed this download", and a seed sitting at 100% with no
// rate is part of that answer.
func racePeersJSON(peers []ltclient.PeerInfo) string {
	type peerSnap struct {
		IP       string  `json:"ip"`
		Port     string  `json:"port"`
		Client   string  `json:"client"`
		DLSpeed  int64   `json:"dl_speed"`
		ULSpeed  int64   `json:"ul_speed"`
		Progress float64 `json:"progress"`
		Flags    string  `json:"flags"`
	}
	// Sorted on a copy: this is the slice GetPeers just returned to the
	// caller, and reordering someone else's data to build a JSON blob is a
	// surprise waiting for the first caller that reads it after us.
	ranked := make([]ltclient.PeerInfo, len(peers))
	copy(ranked, peers)
	sort.Slice(ranked, func(i, j int) bool { return ranked[i].DLRate > ranked[j].DLRate })
	out := make([]peerSnap, 0, raceTimelinePeerRows)
	for i, p := range ranked {
		if i >= raceTimelinePeerRows && p.Progress < 0.8 {
			continue
		}
		out = append(out, peerSnap{
			IP: p.IP, Port: strconv.Itoa(p.Port), Client: p.Client,
			DLSpeed: p.DLRate, ULSpeed: p.ULRate, Progress: p.Progress, Flags: p.Flags,
		})
	}
	data, err := json.Marshal(out)
	if err != nil {
		return "[]"
	}
	return string(data)
}
