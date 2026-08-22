package api

import (
	"encoding/json"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
)

// agentHoardRowMeta returns cached list-row metadata for one agent hoard torrent.
func (s *Server) agentHoardRowMeta(agentName, hash string) (engineID, category string, ok bool) {
	hash = strings.ToLower(hash)
	s.agentRows.mu.RLock()
	defer s.agentRows.mu.RUnlock()
	list := s.agentRows.byAgent[agentName]
	for _, r := range list {
		if h, _ := r["info_hash"].(string); strings.ToLower(h) != hash {
			continue
		}
		engineID, _ = r["agent_engine"].(string)
		category, _ = r["category"].(string)
		return engineID, category, true
	}
	return "", "", false
}

// resolveRemoteDetailTarget locates the remote agent + engine holding a torrent
// for detail/files endpoints. role is "race" or "hoard". agentHint disambiguates
// when the same hash exists locally and on an agent.
func (s *Server) resolveRemoteDetailTarget(hash, agentHint, role string) (*remoteAgent, string, string, bool) {
	hash = strings.ToLower(hash)
	probeRole := func(ra *remoteAgent) (*remoteAgent, string, string, bool) {
		for _, e := range ra.byRole(role) {
			if e.client == nil {
				continue
			}
			st, err := e.client.GetStatus(hash)
			if err != nil || st == nil || st.InfoHash == "" {
				continue
			}
			category := ""
			if role == "hoard" {
				if _, cat, ok := s.agentHoardRowMeta(ra.name, hash); ok {
					category = cat
				}
			}
			if category == "" {
				if cats, err := e.client.TorrentCategories(e.id); err == nil {
					category = cats[hash]
				}
			}
			return ra, e.id, category, true
		}
		return nil, "", "", false
	}

	if want := strings.TrimSpace(agentHint); want != "" && want != "local" {
		ra := s.remoteAgentByName(want)
		if ra == nil {
			return nil, "", "", false
		}
		if role == "hoard" {
			if engineID, category, ok := s.agentHoardRowMeta(want, hash); ok {
				return ra, engineID, category, true
			}
		}
		return probeRole(ra)
	}

	if role == "hoard" {
		if name, engineID, ok := s.agentHoardOwner(hash); ok {
			if ra := s.remoteAgentByName(name); ra != nil {
				_, category, _ := s.agentHoardRowMeta(name, hash)
				return ra, engineID, category, true
			}
		}
	}

	for _, ra := range s.agentsSnapshot() {
		if r, eid, cat, ok := probeRole(ra); ok {
			return r, eid, cat, true
		}
	}
	return nil, "", "", false
}

func (s *Server) resolveHoardDetailTarget(hash, agentHint string) (*remoteAgent, string, string, bool) {
	return s.resolveRemoteDetailTarget(hash, agentHint, "hoard")
}

func (s *Server) resolveRaceDetailTarget(hash, agentHint string) (*remoteAgent, string, string, bool) {
	return s.resolveRemoteDetailTarget(hash, agentHint, "race")
}

func (s *Server) remoteTorrentDetail(ra *remoteAgent, engineID, hash, category string) map[string]interface{} {
	cl, _ := ra.resolveEngine(engineID)
	if cl == nil {
		cl = ra.anyClient()
	}
	if cl == nil {
		return nil
	}
	return buildRemoteTorrentDetail(ra.name, cl, hash, category)
}

func buildRemoteTorrentDetail(agentName string, cl *grpcclient.Client, hash, category string) map[string]interface{} {
	st, err := cl.GetStatus(hash)
	if err != nil || st == nil || st.InfoHash == "" {
		return nil
	}
	var added, completed time.Time
	if st.AddedTime > 0 {
		added = time.Unix(st.AddedTime, 0)
	}
	if st.CompletedTime > 0 {
		completed = time.Unix(st.CompletedTime, 0)
	}
	stats := engine.LtStatusToTorrentStats(*st, category, st.SavePath, added, completed)

	ltPeers, _ := cl.GetPeers(hash)
	peers := make([]engine.PeerInfo, 0, len(ltPeers))
	for _, p := range ltPeers {
		peers = append(peers, engine.LtPeerToPeerInfo(p))
	}

	detail := &engine.TorrentDetail{
		TorrentStats: stats,
		Peers:        peers,
		NumPieces:    st.NumPieces,
		PieceLength:  st.PieceLength,
		SeedingTime:  st.SeedingTime,
		ActiveTime:   st.ActiveTime,
	}
	m := detail.ToMap()
	if m == nil {
		return nil
	}

	if trackers, err := cl.GetTrackers(hash); err == nil {
		trackerMaps := make([]map[string]interface{}, 0, len(trackers))
		for _, t := range trackers {
			tm := map[string]interface{}{
				"url":      t.URL,
				"tier":     t.Tier,
				"verified": t.Verified,
			}
			var endpoints []map[string]interface{}
			if len(t.Endpoints) > 0 {
				_ = json.Unmarshal(t.Endpoints, &endpoints)
			}
			tm["endpoints"] = endpoints
			trackerMaps = append(trackerMaps, tm)
		}
		m["trackers"] = trackerMaps
	}

	m["swarm_seeds"] = st.ListSeeds
	m["swarm_leechers"] = st.ListPeers
	m["torrent_error"] = st.State == "error"
	m["torrent_error_msg"] = st.ErrorMsg
	m["agent"] = agentName
	return m
}

func remoteTorrentFiles(cl *grpcclient.Client, hash string) (files []map[string]interface{}, avail map[string]interface{}) {
	ltFiles, err := cl.GetFiles(hash)
	if err == nil && len(ltFiles) > 0 {
		files = make([]map[string]interface{}, 0, len(ltFiles))
		for _, f := range ltFiles {
			files = append(files, map[string]interface{}{"path": f.Path, "size": f.Size})
		}
	}
	if a, err := cl.GetAvailability(hash); err == nil && a != nil && a.HasPieceMap {
		avail = map[string]interface{}{
			"min":        a.MinAvailability,
			"max":        a.MaxAvailability,
			"avg":        a.AvgAvailability,
			"num_pieces": a.NumPieces,
		}
	}
	return files, avail
}

func (s *Server) torrentDetailFromRemote(agentHint, hash, role string) map[string]interface{} {
	ra, engineID, category, ok := s.resolveRemoteDetailTarget(hash, agentHint, role)
	if !ok {
		return nil
	}
	return s.remoteTorrentDetail(ra, engineID, hash, category)
}

func (s *Server) hoardDetailFromRemote(agentHint, hash string) map[string]interface{} {
	return s.torrentDetailFromRemote(agentHint, hash, "hoard")
}

func (s *Server) raceDetailFromRemote(agentHint, hash string) map[string]interface{} {
	return s.torrentDetailFromRemote(agentHint, hash, "race")
}

func (s *Server) remoteHoardDetail(ra *remoteAgent, engineID, hash, category string) map[string]interface{} {
	return s.remoteTorrentDetail(ra, engineID, hash, category)
}

func (s *Server) filesFromRemote(agentHint, hash, role string) (files []map[string]interface{}, avail map[string]interface{}, ok bool) {
	ra, engineID, _, found := s.resolveRemoteDetailTarget(hash, agentHint, role)
	if !found {
		return nil, nil, false
	}
	cl, _ := ra.resolveEngine(engineID)
	if cl == nil {
		cl = ra.anyClient()
	}
	if cl == nil {
		return nil, nil, false
	}
	files, avail = remoteTorrentFiles(cl, hash)
	return files, avail, true
}
