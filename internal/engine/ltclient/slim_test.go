package ltclient

import (
	"testing"
	"time"
)

// The two projections share a client but must never share a cache slot: a
// caller asking for the full listing that got a slim one would see most of
// TorrentStatus silently zeroed, which reads as "these torrents have no name,
// no tracker and no peers" rather than as an error.
func TestSlimAndFullListingsDoNotShareACacheSlot(t *testing.T) {
	optListCache.Store(true)
	defer optListCache.Store(false)

	c := &Client{}
	now := time.Now()
	c.listCache = &ListTorrentsResult{
		Torrents: []TorrentStatus{{InfoHash: "a", Name: "full-row", NumPeers: 3}},
		Count:    1,
	}
	c.listCachedAt = now
	c.slimCache = &ListTorrentsResult{
		Torrents: []TorrentStatus{{InfoHash: "a"}},
		Count:    1,
	}
	c.slimCachedAt = now

	full := c.cachedListFor(false)
	if full == nil {
		t.Fatal("full listing returned nil")
	}
	if full.Torrents[0].Name != "full-row" || full.Torrents[0].NumPeers != 3 {
		t.Errorf("the full listing was served the slim projection: %+v", full.Torrents[0])
	}

	slim := c.cachedListFor(true)
	if slim == nil {
		t.Fatal("slim listing returned nil")
	}
	if slim.Torrents[0].Name != "" {
		t.Errorf("the slim listing was served the full rows: %+v", slim.Torrents[0])
	}

	// Expiry is per slot too: an expired slim slot must not fall back to a
	// fresh full one.
	c.slimCachedAt = now.Add(-time.Hour)
	if got := c.cachedListFor(true); got != nil {
		t.Errorf("an expired slim slot served %d rows instead of forcing a refetch", len(got.Torrents))
	}
	if c.cachedListFor(false) == nil {
		t.Error("expiring the slim slot also expired the full one")
	}
}

// cachedList stays the full listing: existing callers must not silently change
// projection.
func TestCachedListIsTheFullProjection(t *testing.T) {
	optListCache.Store(true)
	defer optListCache.Store(false)

	c := &Client{}
	c.listCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "a", Name: "full-row"}}, Count: 1}
	c.listCachedAt = time.Now()
	c.slimCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "z"}}, Count: 1}
	c.slimCachedAt = time.Now()

	got := c.cachedList()
	if got == nil || got.Torrents[0].Name != "full-row" {
		t.Errorf("cachedList no longer returns the full projection: %+v", got)
	}
}
