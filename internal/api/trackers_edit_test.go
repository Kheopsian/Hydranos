package api

import (
	"reflect"
	"strings"
	"testing"
)

func tiers(t ...[]string) [][]string { return t }

func TestAddAppendsATierAndNeverDuplicates(t *testing.T) {
	start := tiers([]string{"http://a.invalid/ann"})

	got, changed, err := applyTrackerOp(start, "add", []string{"http://b.invalid/ann"}, "", "")
	if err != nil {
		t.Fatalf("add: %v", err)
	}
	if !changed {
		t.Fatal("adding a new tracker reported no change")
	}
	want := tiers([]string{"http://a.invalid/ann"}, []string{"http://b.invalid/ann"})
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}

	// Adding one we already announce to must be a no-op, and must SAY it changed
	// nothing: a bulk pass uses that answer to skip rewriting the .torrent.
	got2, changed2, err := applyTrackerOp(got, "add", []string{"http://b.invalid/ann"}, "", "")
	if err != nil {
		t.Fatalf("re-add: %v", err)
	}
	if changed2 {
		t.Fatal("adding a tracker that is already there reported a change")
	}
	if !reflect.DeepEqual(got2, got) {
		t.Fatalf("re-add altered the list: %v", got2)
	}
}

func TestRemoveDropsTheUrlAndTheTierItEmptied(t *testing.T) {
	start := tiers(
		[]string{"http://a.invalid/ann", "http://a2.invalid/ann"},
		[]string{"udp://b.invalid:6969"},
	)
	got, changed, err := applyTrackerOp(start, "remove", []string{"udp://b.invalid:6969"}, "", "")
	if err != nil {
		t.Fatalf("remove: %v", err)
	}
	if !changed {
		t.Fatal("removing an existing tracker reported no change")
	}
	want := tiers([]string{"http://a.invalid/ann", "http://a2.invalid/ann"})
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}

	// Removing something absent changes nothing and must say so.
	_, changed2, err := applyTrackerOp(got, "remove", []string{"http://nope.invalid/ann"}, "", "")
	if err != nil {
		t.Fatalf("remove absent: %v", err)
	}
	if changed2 {
		t.Fatal("removing an absent tracker reported a change")
	}
}

// The domain-migration case: the URL changes, its place in the tier order does
// not. A replace that reshuffled tiers would change which tracker is tried
// first on thousands of torrents at once.
func TestReplaceKeepsThePositionInTheTier(t *testing.T) {
	start := tiers(
		[]string{"http://old.invalid/ann", "http://keep.invalid/ann"},
		[]string{"udp://b.invalid:6969"},
	)
	got, changed, err := applyTrackerOp(start, "replace", nil, "http://old.invalid/ann", "http://new.invalid/ann")
	if err != nil {
		t.Fatalf("replace: %v", err)
	}
	if !changed {
		t.Fatal("replace reported no change")
	}
	want := tiers(
		[]string{"http://new.invalid/ann", "http://keep.invalid/ann"},
		[]string{"udp://b.invalid:6969"},
	)
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestReplaceRefusesAUrlTheTorrentDoesNotHave(t *testing.T) {
	start := tiers([]string{"http://a.invalid/ann"})
	_, _, err := applyTrackerOp(start, "replace", nil, "http://ghost.invalid/ann", "http://new.invalid/ann")
	if err == nil {
		t.Fatal("replacing an absent URL was accepted")
	}
	if !strings.Contains(err.Error(), "does not announce") {
		t.Fatalf("unhelpful error: %v", err)
	}
}

func TestSetReplacesEverything(t *testing.T) {
	start := tiers([]string{"http://a.invalid/ann"}, []string{"http://b.invalid/ann"})
	got, changed, err := applyTrackerOp(start, "set", []string{"http://only.invalid/ann"}, "", "")
	if err != nil {
		t.Fatalf("set: %v", err)
	}
	if !changed {
		t.Fatal("set reported no change")
	}
	if !reflect.DeepEqual(got, tiers([]string{"http://only.invalid/ann"})) {
		t.Fatalf("got %v", got)
	}
}

// A tracker URL is a key elsewhere in Hydra: per-tracker counters, passkey
// overrides, the Trackers tab. Silently normalising one here would file the
// edit under a name the rest of the daemon does not use.
func TestUrlsAreValidatedButNotRewritten(t *testing.T) {
	const messy = "http://Tracker.Invalid:8080/ann?passkey=ABC"
	got, err := normalizeTrackerURL("  " + messy + "  ")
	if err != nil {
		t.Fatalf("rejected a valid URL: %v", err)
	}
	if got != messy {
		t.Fatalf("URL was rewritten: %q -> %q", messy, got)
	}
	for _, bad := range []string{"", "   ", "ftp://x.invalid/ann", "not a url at all", "http://"} {
		if _, err := normalizeTrackerURL(bad); err == nil {
			t.Fatalf("accepted %q", bad)
		}
	}
}

func TestUnknownOperationIsRefusedByName(t *testing.T) {
	_, _, err := applyTrackerOp(nil, "delete-everything", nil, "", "")
	if err == nil || !strings.Contains(err.Error(), "unknown operation") {
		t.Fatalf("err = %v", err)
	}
}

// Empty tiers must never survive an edit: the announce loop walks tiers in
// order and an empty one is a level that can never answer.
func TestNoEmptyTierSurvives(t *testing.T) {
	start := tiers([]string{"http://a.invalid/ann"}, []string{"http://b.invalid/ann"})
	got, _, err := applyTrackerOp(start, "remove", []string{"http://a.invalid/ann"}, "", "")
	if err != nil {
		t.Fatalf("remove: %v", err)
	}
	for i, tier := range got {
		if len(tier) == 0 {
			t.Fatalf("tier %d is empty in %v", i, got)
		}
	}
}
