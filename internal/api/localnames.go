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
	LocalAgentName  = "local"
	LocalAgentRace  = "local-race"
	LocalAgentHoard = "local-hoard"
)

// isLocalAgentName reports whether a name refers to an engine of this process.
//
// Every caller that used to compare against "local" goes through here, because
// the comparison means "do I dispatch in process or over the wire" and that
// answer must not change with the rename. Missing one would send a local add,
// pause or bulk action looking for an agent that was never dialled.
func isLocalAgentName(n string) bool {
	switch n {
	case LocalAgentName, LocalAgentRace, LocalAgentHoard:
		return true
	}
	return false
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
