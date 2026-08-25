package api

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
)

// agentStore is the on-disk (agents.json), UI-managed remote-agent config. It is
// the counterpart of the toml [[agent]] block, but mutable at runtime from the
// Agents menu (add/edit/remove without a restart).
type agentStore struct {
	Addr  string `json:"addr"`
	Token string `json:"token,omitempty"`
	TLSCa string `json:"tls_ca,omitempty"`
}

func agentsFile(dataDir string) string { return filepath.Join(dataDir, "agents.json") }

func loadAgentStore(dataDir string) map[string]agentStore {
	data, err := os.ReadFile(agentsFile(dataDir))
	if err != nil {
		return map[string]agentStore{}
	}
	var m map[string]agentStore
	if json.Unmarshal(data, &m) != nil {
		return map[string]agentStore{}
	}
	return m
}

func removedFile(dataDir string) string { return filepath.Join(dataDir, "agents_removed.json") }

func loadRemovedStore(dataDir string) map[string]agentStore {
	data, err := os.ReadFile(removedFile(dataDir))
	if err != nil {
		return map[string]agentStore{}
	}
	var m map[string]agentStore
	if json.Unmarshal(data, &m) != nil {
		return map[string]agentStore{}
	}
	return m
}

func saveRemovedStore(dataDir string, m map[string]agentStore) error {
	data, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	tmp := removedFile(dataDir) + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, removedFile(dataDir))
}

func saveAgentStore(dataDir string, m map[string]agentStore) error {
	data, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	tmp := agentsFile(dataDir) + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, agentsFile(dataDir))
}

// agentsSnapshot returns the REMOTE agents only, copied under RLock so
// iterating callers never race a runtime add/remove.
//
// Excluding this node's own agent is the safe default, and it is deliberate.
// Every one of the dozen callers was written when "local" was not an agent, so
// each one that wants the local contribution already adds it separately.
// Registering the local engines (3.135.0) made this function return them too,
// and every such caller started counting them twice: /api/status reported
// 396592 torrents for 198296 rows, and the race listing doubled as well. It was
// fixed at one collector first, which was whack-a-mole -- three more sites had
// exactly the same shape.
//
// So the default is what every existing caller already assumes, and the two
// places that genuinely want this node in the list ask for it by name.
func (s *Server) agentsSnapshot() []*remoteAgent {
	all := s.allAgentsSnapshot()
	out := all[:0]
	for _, ra := range all {
		if !ra.local {
			out = append(out, ra)
		}
	}
	return out
}

// allAgentsSnapshot includes this node's own agent. Only for callers that
// present or resolve agents BY NAME -- the agents list and the agent detail
// view -- never for anything that aggregates, which is where double counting
// comes from.
func (s *Server) allAgentsSnapshot() []*remoteAgent {
	s.agentsMu.RLock()
	defer s.agentsMu.RUnlock()
	out := make([]*remoteAgent, len(s.remoteAgents))
	copy(out, s.remoteAgents)
	return out
}

// removeRemoteAgentLocked drops an agent and closes its clients. Caller holds agentsMu.
func (s *Server) removeRemoteAgentLocked(name string) bool {
	for i, ra := range s.remoteAgents {
		if ra.name == name {
			for _, e := range ra.engines {
				if e.client != nil {
					e.client.Close()
				}
			}
			s.remoteAgents = append(s.remoteAgents[:i], s.remoteAgents[i+1:]...)
			return true
		}
	}
	return false
}

// RemoveRemoteAgent unregisters an agent and closes its clients.
func (s *Server) RemoveRemoteAgent(name string) bool {
	s.agentsMu.Lock()
	defer s.agentsMu.Unlock()
	return s.removeRemoteAgentLocked(name)
}

// findRemoteOwner locates which remote agent + mode holds a torrent, by probing
// GetStatus (hoard first). Used to auto-route a per-torrent action when the
// torrent is not local — no UI plumbing needed.
func (s *Server) findRemoteOwner(hash string) (*remoteAgent, string, bool) {
	for _, ra := range s.agentsSnapshot() {
		for _, e := range ra.engines {
			if st, err := e.client.GetStatus(hash); err == nil && st != nil && st.InfoHash != "" {
				return ra, e.id, true
			}
		}
	}
	return nil, "", false
}

// agentReq is the create/update/test payload from the Agents menu.
type agentReq struct {
	Name  string `json:"name"`
	Addr  string `json:"addr"`
	Token string `json:"token"`
	TLSCa string `json:"tls_ca"`
}

func (s *Server) handleAgentCreate(c *gin.Context) {
	var req agentReq
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.Name == "" || req.Addr == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "name and addr are required"})
		return
	}
	if isReservedAgentName(req.Name) {
		c.JSON(http.StatusBadRequest, gin.H{"error": `"local" is reserved`})
		return
	}
	// Dial first: a bad agent is rejected up front, never persisted.
	if err := s.AddRemoteAgent(req.Name, req.Addr, req.Token, req.TLSCa); err != nil {
		c.JSON(http.StatusBadGateway, gin.H{"error": "dial failed: " + err.Error()})
		return
	}
	m := loadAgentStore(s.config.Daemon.DataDir)
	m[req.Name] = agentStore{Addr: req.Addr, Token: req.Token, TLSCa: req.TLSCa}
	if err := saveAgentStore(s.config.Daemon.DataDir, m); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusCreated, gin.H{"status": "ok"})
}

func (s *Server) handleAgentUpdate(c *gin.Context) {
	name := c.Param("name")
	var req agentReq
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.Addr == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "addr is required"})
		return
	}
	// AddRemoteAgent replaces a same-name agent (closes the old clients, re-dials).
	if err := s.AddRemoteAgent(name, req.Addr, req.Token, req.TLSCa); err != nil {
		c.JSON(http.StatusBadGateway, gin.H{"error": "dial failed: " + err.Error()})
		return
	}
	m := loadAgentStore(s.config.Daemon.DataDir)
	m[name] = agentStore{Addr: req.Addr, Token: req.Token, TLSCa: req.TLSCa}
	if err := saveAgentStore(s.config.Daemon.DataDir, m); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

func (s *Server) handleAgentDelete(c *gin.Context) {
	name := c.Param("name")
	s.RemoveRemoteAgent(name) // close live clients (agent + its data untouched)
	m := loadAgentStore(s.config.Daemon.DataDir)
	// Soft-delete: park the config in the removed store so an accidental
	// delete is one click to restore. The remote agent keeps running.
	entry, ok := m[name]
	if !ok {
		// A [[agent]] block from the TOML never lands in agents.json, so the
		// removed store is the only place a delete can be recorded for it.
		// Without that record the reconnect loop dials it again a minute later
		// and the delete undoes itself.
		for _, ag := range s.config.Agents {
			if ag.Name == name {
				entry, ok = agentStore{Addr: ag.Addr, Token: ag.Token, TLSCa: ag.TLSCa}, true
				break
			}
		}
	}
	if ok {
		rm := loadRemovedStore(s.config.Daemon.DataDir)
		rm[name] = entry
		saveRemovedStore(s.config.Daemon.DataDir, rm)
		delete(m, name)
	}
	if err := saveAgentStore(s.config.Daemon.DataDir, m); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

// handleAgentsRemovedGet lists soft-deleted agents (restore candidates).
func (s *Server) handleAgentsRemovedGet(c *gin.Context) {
	rm := loadRemovedStore(s.config.Daemon.DataDir)
	out := []agentInfo{}
	for name, a := range rm {
		out = append(out, agentInfo{Name: name, Kind: "grpc", Online: false, Addr: a.Addr})
	}
	c.JSON(http.StatusOK, out)
}

// handleAgentRestore un-deletes an agent: it always comes back to the live
// config (so nothing is lost); the re-dial is best-effort (a currently-down
// agent still gets restored and connects when it is back).
func (s *Server) handleAgentRestore(c *gin.Context) {
	name := c.Param("name")
	rm := loadRemovedStore(s.config.Daemon.DataDir)
	entry, ok := rm[name]
	if !ok {
		c.JSON(http.StatusNotFound, gin.H{"error": "not in removed list"})
		return
	}
	m := loadAgentStore(s.config.Daemon.DataDir)
	m[name] = entry
	if err := saveAgentStore(s.config.Daemon.DataDir, m); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	delete(rm, name)
	saveRemovedStore(s.config.Daemon.DataDir, rm)
	if err := s.AddRemoteAgent(name, entry.Addr, entry.Token, entry.TLSCa); err != nil {
		c.JSON(http.StatusOK, gin.H{"status": "ok", "warning": "restored (config) but dial failed: " + err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

// handleAgentTest dials an agent without persisting or registering it, so the
// menu can validate address/token/CA before saving.
func (s *Server) handleAgentTest(c *gin.Context) {
	var req agentReq
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.Addr == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "addr is required"})
		return
	}
	cl, err := grpcclient.New(grpcclient.Config{Addr: req.Addr, Engine: agentwire.EngineRace, Token: req.Token, TLSCa: req.TLSCa})
	if err != nil {
		c.JSON(http.StatusOK, gin.H{"online": false, "error": err.Error()})
		return
	}
	cl.Close()
	c.JSON(http.StatusOK, gin.H{"online": true})
}
