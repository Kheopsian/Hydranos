package api

import "strings"

// The names this node answers to.
//
// LocalAgentName is the historical one and stays valid forever: it is written
// into existing categories, save-path overrides and job params, and breaking it
// would rewrite people's configuration for a rename. It means "this node,
// whichever engine the mode picks".
//
// The per-role names are what one agent per engine needs: they address ONE
// engine, so a category can send race torrents to one tunnel and hoard torrents
// to another on the same machine.
const (
	LocalAgentName = "local"
	// LocalAgentPrefix marks every name this node answers to beyond the bare
	// alias. Reserved against dialled agents, so it can be trusted as proof
	// that a name is ours.
	LocalAgentPrefix = "local-"
	LocalAgentRace   = "local-race"
	LocalAgentHoard  = "local-hoard"
)

// isLocalAgentName reports whether a name refers to an engine of this process.
//
// Every caller that used to compare against "local" goes through here, because
// the comparison means "do I dispatch in process or over the wire" and that
// answer must not change with the rename. Missing one would send a local add,
// pause or bulk action looking for an agent that was never dialled.
func isLocalAgentName(n string) bool {
	if n == LocalAgentName {
		return true
	}
	// Any "local-" name is one of this node's engines. The two primaries are
	// local-race and local-hoard; an extra engine created from the UI takes its
	// own id, so a shard called "race-2" is the agent "local-race-2".
	//
	// A prefix rather than a fixed list because those ids are chosen by the
	// operator and cannot be enumerated here. It is safe precisely because the
	// same prefix is refused to dialled agents (isReservedAgentName): nothing
	// remote can ever claim a name that would make its actions run here.
	return strings.HasPrefix(n, LocalAgentPrefix)
}

// isPrimaryLocalAgent names the two engines this daemon is built around, whose
// torrents the LOCAL path already reports -- the list handlers read them
// straight from raceEngine/hoardEngine. Every other engine of this node is
// reported by nobody else, so it has to be collected like an agent.
//
// This is the whole of the double-counting rule, in one place. 3.135.0 shipped
// without it and /api/status reported 396592 torrents for the 198296 the
// database held, because registering the local engines enrolled them in the
// aggregate they were already part of.
func isPrimaryLocalAgent(n string) bool {
	return n == LocalAgentRace || n == LocalAgentHoard || n == LocalAgentName
}

// LocalAgentNameFor builds the agent name of one of this node's engines.
func LocalAgentNameFor(engineID string) string {
	switch engineID {
	case "race", "hoard":
		return LocalAgentPrefix + engineID
	}
	return LocalAgentPrefix + strings.TrimSpace(engineID)
}

// localAgentForRole names the engine of this node that serves a role.
func localAgentForRole(role string) string {
	if role == "race" {
		return LocalAgentRace
	}
	return LocalAgentHoard
}

// roleOfLocalAgent returns the engine role a per-role local name pins, and
// whether the name pinned one at all. The bare "local" pins nothing: it means
// whichever engine the caller's mode selects, which is how it has always
// behaved and what existing configs rely on.
func roleOfLocalAgent(n string) (string, bool) {
	switch n {
	case LocalAgentRace:
		return "race", true
	case LocalAgentHoard:
		return "hoard", true
	}
	return "", false
}

// isReservedAgentName keeps a dialled agent from taking a name this node
// answers to. Without it someone could add a remote agent called "local-race"
// and every action meant for it would be executed here instead, silently and on
// the wrong machine.
func isReservedAgentName(n string) bool {
	return isLocalAgentName(strings.TrimSpace(n))
}

// roleOfLocalAgentIn resolves the engine role a local agent name pins, using
// the registry for names this file cannot decode on its own -- an extra engine
// carries an operator-chosen id, so "local-seedbox-2" says nothing about its
// role until the registry is asked.
func (s *Server) roleOfLocalAgentIn(name string) (string, bool) {
	if role, ok := roleOfLocalAgent(name); ok {
		return role, true
	}
	if !isLocalAgentName(name) || name == LocalAgentName {
		return "", false
	}
	ra := s.remoteAgentByName(name)
	if ra == nil || len(ra.engines) == 0 {
		return "", false
	}
	return ra.engines[0].role, true
}
