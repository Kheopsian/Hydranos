package api

import (
	"testing"
)

func TestAgentHoardRowMeta(t *testing.T) {
	s := &Server{}
	s.agentRows.byAgent = map[string][]map[string]interface{}{
		"seedbox": {
			{"info_hash": "AA", "agent_engine": "hoard-0", "category": "movies"},
		},
	}
	engineID, cat, ok := s.agentHoardRowMeta("seedbox", "aa")
	if !ok || engineID != "hoard-0" || cat != "movies" {
		t.Fatalf("meta = (%q, %q, %v), want (hoard-0, movies, true)", engineID, cat, ok)
	}
	if _, _, ok := s.agentHoardRowMeta("seedbox", "zz"); ok {
		t.Fatal("unknown hash reported as found")
	}
}

func TestResolveHoardDetailTargetFromCache(t *testing.T) {
	s := &Server{}
	s.agentRows.byAgent = map[string][]map[string]interface{}{
		"seedbox": {
			{"info_hash": "aa", "agent_engine": "hoard-0", "category": "movies"},
		},
	}
	s.remoteAgents = []*remoteAgent{{name: "seedbox", engines: []remoteEngine{{id: "hoard-0"}}}}

	ra, engineID, cat, ok := s.resolveHoardDetailTarget("aa", "")
	if !ok || ra.name != "seedbox" || engineID != "hoard-0" || cat != "movies" {
		t.Fatalf("resolve = (%q, %q, %q, %v)", ra.name, engineID, cat, ok)
	}
}

func TestResolveHoardDetailTargetWithAgentHint(t *testing.T) {
	s := &Server{}
	s.agentRows.byAgent = map[string][]map[string]interface{}{
		"seedbox": {
			{"info_hash": "aa", "agent_engine": "hoard-0", "category": "tv"},
		},
	}
	s.remoteAgents = []*remoteAgent{{name: "seedbox", engines: []remoteEngine{{id: "hoard-0"}}}}

	ra, engineID, cat, ok := s.resolveHoardDetailTarget("aa", "seedbox")
	if !ok || ra.name != "seedbox" || engineID != "hoard-0" || cat != "tv" {
		t.Fatalf("resolve with hint = (%q, %q, %q, %v)", ra.name, engineID, cat, ok)
	}
	if _, _, _, ok := s.resolveHoardDetailTarget("aa", "missing"); ok {
		t.Fatal("unknown agent hint should not resolve")
	}
	if _, _, _, ok := s.resolveHoardDetailTarget("aa", "seedbox"); !ok {
		t.Fatal("expected seedbox to hold aa")
	}
	if _, _, _, ok := s.resolveHoardDetailTarget("zz", "seedbox"); ok {
		t.Fatal("hash not on hinted agent should not resolve from cache")
	}
}

func TestResolveRemoteDetailTargetEmpty(t *testing.T) {
	s := &Server{}
	if _, _, _, ok := s.resolveRemoteDetailTarget("aa", "", "race"); ok {
		t.Fatal("empty server should not resolve race")
	}
	if _, _, _, ok := s.resolveHoardDetailTarget("aa", ""); ok {
		t.Fatal("empty server should not resolve hoard")
	}
}
