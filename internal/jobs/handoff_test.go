package jobs

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

func (f *fakeSource) StartTorrent(string) error {
	*f.log = append(*f.log, "source.start")
	return nil
}

// handoffRig points both ends at the same path -- which is what two engines of
// one node share -- and asks for a handoff.
func handoffRig(t *testing.T, mode bool) *rig {
	t.Helper()
	r := newRig(t, RemoteMoveParams{}, 1<<30)
	var p RemoteMoveParams
	_ = json.Unmarshal([]byte(r.job.Params), &p)
	p.Handoff = true
	p.ReleaseSource = mode
	p.SourceAgent, p.TargetAgent = "local-hoard", "local-vpn7"
	blob, _ := json.Marshal(p)
	r.job.Params = string(blob)
	r.src.rec.SavePath = "/target/films" // already where the target expects it
	return r
}

// The payload never moves: the two engines share a filesystem, so copying would
// have the target write the very files the source is reading, and the release
// at the end would delete what the target now points at.
func TestHandoffMovesNoBytesAndNeverDeletesTheData(t *testing.T) {
	r := handoffRig(t, true)
	if err := r.run(context.Background()); err != nil {
		t.Fatalf("run: %v", err)
	}
	if len(r.sink.written) != 0 {
		t.Errorf("wrote %d pieces for a handoff: the target overwrote the files it was given", len(r.sink.written))
	}
	if r.src.reads != 0 {
		t.Errorf("read %d pieces for a handoff", r.src.reads)
	}
	if !r.src.removed || !r.src.keptData {
		t.Errorf("source released with keepData=%v (removed=%v): the payload belongs to the target now",
			r.src.keptData, r.src.removed)
	}
	if !r.sink.started {
		t.Error("the target was never started")
	}
	// The source must stop BEFORE the target adopts: never two engines with the
	// same files open for writing.
	order := strings.Join(r.log, ",")
	if !strings.HasPrefix(order, "source.stop,sink.adopt") {
		t.Errorf("order = %q, want the source stopped before the target adopts", order)
	}
	if strings.Contains(order, "sink.verify") {
		t.Errorf("rechecked the whole payload for a handoff: %q", order)
	}
}

// A handoff whose two paths differ is a copy wearing the wrong name: the target
// would be handed an empty directory it believes is complete.
func TestHandoffRefusesWhenThePathsDiffer(t *testing.T) {
	r := handoffRig(t, true)
	r.src.rec.SavePath = "/somewhere/else"
	err := r.run(context.Background())
	if err == nil {
		t.Fatal("a handoff across two different paths was accepted")
	}
	if r.src.stopped || r.src.removed {
		t.Errorf("the source was touched before the refusal: %+v", r.log)
	}
}

// Two engines seeding one set of files are two writers on the same bytes the
// first time either repairs a piece.
func TestHandoffRefusesToDuplicate(t *testing.T) {
	r := handoffRig(t, false)
	if err := r.run(context.Background()); err == nil {
		t.Fatal("a handoff was allowed to duplicate")
	}
	if r.src.stopped {
		t.Error("the source was stopped for a refused handoff")
	}
}

// A failure between the two ends must not leave the torrent stopped everywhere.
func TestHandoffPutsTheSourceBackWhenTheTargetRefuses(t *testing.T) {
	r := handoffRig(t, true)
	r.runner.DialSink = func(RemoteMoveParams) (PieceSink, error) {
		return adoptRefusingSink{r.sink}, nil
	}
	if err := r.run(context.Background()); err == nil {
		t.Fatal("expected the adopt failure to surface")
	}
	if !strings.Contains(strings.Join(r.log, ","), "source.start") {
		t.Errorf("the source was left stopped after a failed handoff: %+v", r.log)
	}
	if r.src.removed {
		t.Error("the source was released even though the target never took it")
	}
}

// adoptRefusingSink is a target that cannot take the torrent: everything else
// behaves, so the test measures the recovery and not a broken stub.
type adoptRefusingSink struct{ *fakeSink }

func (adoptRefusingSink) ImportStateWithFile(*ltclient.ResumeRecord, []byte) (string, error) {
	return "", fmt.Errorf("target refused the adopt")
}
