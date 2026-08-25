package api

import "testing"

// TestRowKeyKeepsOneTorrentPerEngine guards the one semantic the storage
// refactor could have changed by accident. An agent can host several hoard
// engines, and the same torrent can legitimately sit on two of them. The slice
// this cache used to be kept both rows; a map keyed on the hash alone would
// silently collapse them, changing what the table shows and what the header
// counts -- the same class of quiet arithmetic error that doubled the counts
// earlier today, in the other direction.
func TestRowKeyKeepsOneTorrentPerEngine(t *testing.T) {
	set := rowSet{}
	set[rowKey("hoard-0", "AA")] = map[string]interface{}{"info_hash": "AA", "agent_engine": "hoard-0"}
	set[rowKey("hoard-1", "aa")] = map[string]interface{}{"info_hash": "aa", "agent_engine": "hoard-1"}
	if len(set) != 2 {
		t.Fatalf("%d row(s) for one torrent on two engines, want 2: the second engine's copy was swallowed", len(set))
	}
	// Same engine, same torrent, different case: one row, not two.
	set[rowKey("hoard-0", "aa")] = map[string]interface{}{"info_hash": "aa", "agent_engine": "hoard-0"}
	if len(set) != 2 {
		t.Errorf("%d rows: the same torrent on the same engine was duplicated by letter case", len(set))
	}
	if len(set.rows()) != 2 {
		t.Errorf("rows() flattened to %d", len(set.rows()))
	}
}

// An empty or absent set must flatten to nothing rather than panic: the cache
// is read before it has ever been filled on every cold start.
func TestEmptyRowSetFlattensToNothing(t *testing.T) {
	var nilSet rowSet
	if r := nilSet.rows(); r != nil {
		t.Errorf("nil set produced %v", r)
	}
	if r := (rowSet{}).rows(); r != nil {
		t.Errorf("empty set produced %v", r)
	}
}
