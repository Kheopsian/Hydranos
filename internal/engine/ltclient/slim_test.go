package ltclient

import (
	"testing"
	"time"
)

// A full row carries every field a slim caller reads, so a fresh full snapshot
// answers a slim request. The reverse is never true: handing a full-listing
// caller a slim row would blank most of TorrentStatus silently.
func TestSlimRequestSharesTheFullSnapshot(t *testing.T) {
	optListCache.Store(true)
	defer optListCache.Store(false)

	c := &Client{}
	c.listCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "a", Name: "full-row"}}, Count: 1}
	c.listCachedAt = time.Now()

	got := c.cachedListFor(true)
	if got == nil {
		t.Fatal("a slim request refused a fresh full snapshot and will refetch")
	}
	if got.Torrents[0].Name != "full-row" {
		t.Errorf("expected the shared full snapshot, got %+v", got.Torrents[0])
	}
}

func TestFullRequestIsNeverServedSlimRows(t *testing.T) {
	optListCache.Store(true)
	defer optListCache.Store(false)

	c := &Client{}
	c.slimCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "a"}}, Count: 1}
	c.slimCachedAt = time.Now()
	// No full snapshot at all: the request must refetch, not borrow the slim.
	if got := c.cachedListFor(false); got != nil {
		t.Errorf("full request was served the slim projection: %+v", got.Torrents)
	}
	if got := c.cachedList(); got != nil {
		t.Errorf("cachedList was served the slim projection: %+v", got.Torrents)
	}
}

// With the full snapshot stale, a slim request falls back to its own slot.
func TestSlimFallsBackToItsOwnSlot(t *testing.T) {
	optListCache.Store(true)
	defer optListCache.Store(false)

	c := &Client{}
	c.listCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "a", Name: "full-row"}}, Count: 1}
	c.listCachedAt = time.Now().Add(-time.Hour)
	c.slimCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "b"}}, Count: 1}
	c.slimCachedAt = time.Now()

	got := c.cachedListFor(true)
	if got == nil || got.Torrents[0].InfoHash != "b" {
		t.Errorf("slim did not fall back to its own slot: %+v", got)
	}
	if c.cachedListFor(false) != nil {
		t.Error("a stale full snapshot was served anyway")
	}
}

// The slim slot outlives the loops' period on purpose: at a TTL below 10s a
// 10s loop misses its own previous snapshot every single firing.
func TestSlimTTLOutlivesTheLoopPeriod(t *testing.T) {
	if slimCacheTTL <= 10*time.Second {
		t.Fatalf("slimCacheTTL = %v, must exceed the 10s loop period or every firing refetches", slimCacheTTL)
	}

	optListCache.Store(true)
	defer optListCache.Store(false)
	c := &Client{}
	c.slimCache = &ListTorrentsResult{Torrents: []TorrentStatus{{InfoHash: "a"}}, Count: 1}
	// A snapshot taken one loop period ago must still be good.
	c.slimCachedAt = time.Now().Add(-10 * time.Second)
	if c.cachedListFor(true) == nil {
		t.Error("a 10s-old slim snapshot was rejected: consecutive firings will not share")
	}
	// Well past the TTL it must not be.
	c.slimCachedAt = time.Now().Add(-slimCacheTTL - time.Second)
	if c.cachedListFor(true) != nil {
		t.Error("an expired slim snapshot was served")
	}
}
