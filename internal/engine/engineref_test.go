package engine

import (
	"errors"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// countingClient stands for one generation of engine process.
type countingClient struct {
	EngineClient // nil: any method this test does not stub would panic, which is the point
	gen          int
	pings        int
}

func (c *countingClient) Ping() error { c.pings++; return nil }
func (c *countingClient) GetStatus(string) (*ltclient.TorrentStatus, error) {
	return &ltclient.TorrentStatus{Name: string(rune('A' + c.gen))}, nil
}

// TestHoldersFollowASwap is the whole reason EngineRef exists. A client does
// not survive its process -- ltclient dials once and never redials -- so a
// holder that kept a copy goes on writing into a closed socket, believing it is
// connected. The tracker announcers are among those holders, which is how a
// reload could leave us silently announcing to nobody.
func TestHoldersFollowASwap(t *testing.T) {
	gen0 := &countingClient{gen: 0}
	ref := NewEngineRef(gen0)

	// A holder that took the ref at boot, exactly like an announcer does.
	var holder EngineClient = ref

	if err := holder.Ping(); err != nil {
		t.Fatal(err)
	}
	if gen0.pings != 1 {
		t.Fatalf("first generation saw %d pings, want 1", gen0.pings)
	}

	gen1 := &countingClient{gen: 1}
	old := ref.Swap(gen1)
	if old != EngineClient(gen0) {
		t.Error("Swap did not return the previous client, so the caller cannot close it")
	}

	if err := holder.Ping(); err != nil {
		t.Fatal(err)
	}
	if gen1.pings != 1 {
		t.Errorf("the holder is still talking to the dead generation: gen1 saw %d pings", gen1.pings)
	}
	if gen0.pings != 1 {
		t.Errorf("the old generation received a call after the swap: %d pings", gen0.pings)
	}

	st, err := holder.GetStatus("x")
	if err != nil {
		t.Fatal(err)
	}
	if st.Name != "B" {
		t.Errorf("holder answered from generation %q, want the new one", st.Name)
	}
}

// A ref used before its first client is installed must report, not panic. A nil
// dereference here would take the daemon down during the reload window, which
// is precisely when the ref is empty.
func TestEmptyRefReportsInsteadOfPanicking(t *testing.T) {
	ref := &EngineRef{}
	if err := ref.Ping(); err == nil {
		t.Error("an empty ref answered Ping without error")
	}
	if _, err := ref.ListTorrents(); err == nil {
		t.Error("an empty ref answered ListTorrents without error")
	}
	// Close is the exception: closing something that was never opened is a
	// no-op, not a failure, so shutdown paths do not have to special-case it.
	if err := ref.Close(); err != nil {
		t.Errorf("Close on an empty ref returned %v", err)
	}
	var target refNotReady
	if err := ref.Ping(); !errors.As(err, &target) {
		t.Errorf("unexpected error type: %v", err)
	}
}
