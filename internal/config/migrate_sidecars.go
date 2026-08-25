package config

import (
	"fmt"
	"os"
	"strconv"
)

// Folding the UI's sidecar files into the [[agent]] array.
//
// Three files described the same thing for different readers: engines.json for
// engines the UI created, agents.json for nodes it dialled, and the TOML for
// everything a human wrote. Which one won depended on which code path you were
// in, and the Network tab wrote to a fourth place.
//
// This moves the first two into the array and stops there. It is deliberately
// one-way and deliberately additive: nothing is deleted from the TOML, and the
// sidecars are kept under a .migrated name rather than removed, because the
// cost of being wrong here is an install that loses its engines.

// migratedSuffix marks a sidecar that has been folded in. Its presence is what
// stops the migration running twice and appending duplicates.
const migratedSuffix = ".migrated"

// MigrateSidecars folds engines.json and agents.json into [[agent]] entries in
// the config document, returning the new document and what it changed.
//
// It never removes an entry that is already there: SetTOMLArrayTable selects by
// name, so a second run edits in place instead of appending. That matters more
// than it looks -- a half-failed migration that is retried must converge, not
// grow the file every boot.
func MigrateSidecars(doc, dataDir string) (string, []string, error) {
	var done []string

	extras, err := LoadExtraEngines(dataDir)
	if err != nil {
		return doc, nil, fmt.Errorf("read engines.json: %w", err)
	}
	for _, e := range extras {
		name := "local-" + e.ID
		// Dotted keys, not a nested [agent.session] table. A table header
		// would be written once for the whole file, so a second extra engine
		// would land its session in the FIRST agent's block -- one node
		// silently configured with another's port. Dotted keys stay inside the
		// entry they are written to.
		//
		// Only what belongs to THIS engine is written. The rest comes from the
		// role profile, which is where it should have come from all along:
		// engines.json held a frozen copy of the primary's entire config, taken
		// once at creation, and went stale the moment anything changed. Three
		// keys instead of forty is the fix, not a shortcut.
		kv := [][2]string{
			{"role", strconv.Quote(e.Role)},
			{"engine_id", strconv.Quote(e.ID)},
			{"session.listen_port", strconv.Itoa(e.ListenPort)},
			{"session.bind_interface", strconv.Quote(e.BindInterface)},
			{"session.enable_ipv6", strconv.FormatBool(e.EnableIPv6)},
		}
		doc, err = SetTOMLArrayTable(doc, "agent", "name", name, kv)
		if err != nil {
			return doc, done, fmt.Errorf("write agent %q: %w", name, err)
		}
		done = append(done, "engine "+e.ID+" -> [[agent]] "+name)
	}
	return doc, done, nil
}

// MarkSidecarMigrated renames a sidecar out of the way once its contents are
// safely in the TOML. Renamed, not deleted: if the fold turns out to be wrong
// the original is still there, and an install that loses its engines is a much
// worse outcome than a leftover file.
func MarkSidecarMigrated(dataDir, file string) error {
	src := dataDir + "/" + file
	if _, err := os.Stat(src); err != nil {
		return nil // nothing to move
	}
	return os.Rename(src, src+migratedSuffix)
}
