package store

import (
	"os"
	"path/filepath"
	"testing"
)

func openAgentTmp(t *testing.T, name string) *AgentStore {
	t.Helper()
	s, err := OpenAgent(filepath.Join(t.TempDir(), name))
	if err != nil {
		t.Fatalf("openagent: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

func TestAgentPutGetAllDelete(t *testing.T) {
	s := openAgentTmp(t, "a.db")
	r := &AgentRecord{InfoHash: "aa", Torrent: []byte("blob"), SavePath: "/p", Category: "c", CompletedTime: 9}
	if err := s.Put(r); err != nil {
		t.Fatal(err)
	}
	// Same hash again = upsert, not a second row (no session to collide on).
	if err := s.Put(r); err != nil {
		t.Fatal(err)
	}
	if n, _ := s.Count(); n != 1 {
		t.Fatalf("want 1 row, got %d", n)
	}
	got, ok, _ := s.Get("aa")
	if !ok || string(got.Torrent) != "blob" || got.CompletedTime != 9 {
		t.Fatalf("roundtrip: %+v ok=%v", got, ok)
	}
	all, _ := s.All()
	if len(all) != 1 {
		t.Fatalf("All want 1, got %d", len(all))
	}
	if err := s.Delete("aa"); err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := s.Get("aa"); ok {
		t.Fatal("expected deleted")
	}
}

func TestAgentReconcile(t *testing.T) {
	dir := t.TempDir()
	blob := filepath.Join(dir, "b.torrent")
	os.WriteFile(blob, []byte("d...e"), 0644)
	s, err := OpenAgent(filepath.Join(dir, "a.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	// insert a (blob ok), b missing.
	r, err := s.Reconcile([]AgentSyncItem{
		{InfoHash: "a", TorrentFilePath: blob, Category: "c1", CompletedTime: 1},
		{InfoHash: "b", TorrentFilePath: filepath.Join(dir, "gone")},
	})
	if err != nil {
		t.Fatal(err)
	}
	if r.Inserted != 1 || r.Missing != 1 {
		t.Fatalf("want ins=1 miss=1, got %+v", r)
	}
	// update a (blob untouched), delete none.
	r, _ = s.Reconcile([]AgentSyncItem{{InfoHash: "a", TorrentFilePath: blob, Category: "MOVED", CompletedTime: 2}})
	if r.Updated != 1 {
		t.Fatalf("want upd=1, got %+v", r)
	}
	got, _, _ := s.Get("a")
	if got.Category != "MOVED" || string(got.Torrent) != "d...e" {
		t.Fatalf("update-in-place blob intact: %+v", got)
	}
	// empty report must refuse wipe.
	if _, err := s.Reconcile(nil); err == nil {
		t.Fatal("expected refuse-wipe")
	}
	if n, _ := s.Count(); n != 1 {
		t.Fatalf("intact after refused wipe, got %d", n)
	}
	// 'a' gone from engine -> deleted.
	os.WriteFile(filepath.Join(dir, "c.torrent"), []byte("c"), 0644)
	r, _ = s.Reconcile([]AgentSyncItem{{InfoHash: "c", TorrentFilePath: filepath.Join(dir, "c.torrent")}})
	if r.Deleted != 1 || r.Inserted != 1 {
		t.Fatalf("want del=1 ins=1, got %+v", r)
	}
}

func TestSplitLegacyDB(t *testing.T) {
	dir := t.TempDir()
	// Build a legacy session-keyed store with 2 hoard + 1 race.
	legacy, err := Open(filepath.Join(dir, "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	must := func(e error) {
		if e != nil {
			t.Fatal(e)
		}
	}
	must(legacy.Put(&Record{InfoHash: "h1", Session: Hoard, Torrent: []byte("x"), Category: "c"}))
	must(legacy.Put(&Record{InfoHash: "h2", Session: Hoard, Torrent: []byte("y")}))
	must(legacy.Put(&Record{InfoHash: "r1", Session: Race, Torrent: []byte("z"), SavePath: "/r"}))
	legacy.Close()

	race, err := OpenAgent(filepath.Join(dir, "race.db"))
	must(err)
	defer race.Close()
	hoard, err := OpenAgent(filepath.Join(dir, "hoard.db"))
	must(err)
	defer hoard.Close()

	rn, hn, err := SplitLegacyDB(filepath.Join(dir, "hydra.db"), race, hoard)
	must(err)
	if rn != 1 || hn != 2 {
		t.Fatalf("split want race=1 hoard=2, got race=%d hoard=%d", rn, hn)
	}
	if n, _ := race.Count(); n != 1 {
		t.Fatalf("race.db want 1, got %d", n)
	}
	if n, _ := hoard.Count(); n != 2 {
		t.Fatalf("hoard.db want 2, got %d", n)
	}
	got, ok, _ := race.Get("r1")
	if !ok || got.SavePath != "/r" || string(got.Torrent) != "z" {
		t.Fatalf("r1 migrated wrong: %+v", got)
	}
}
