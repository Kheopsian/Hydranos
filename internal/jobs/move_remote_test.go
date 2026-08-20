package jobs

import (
	"context"
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/store"
)

// ── fixtures ────────────────────────────────────────────────────────────────

const testIH = "0fe93c1239226d7e41aedbab3f1fc987c289de51"

// buildTorrent returns a real metafile whose hashes match data, so the runner
// parses a genuine layout rather than a stub.
func buildTorrent(data []byte, pieceLen int64) []byte {
	var pieces []byte
	for off := int64(0); off < int64(len(data)); off += pieceLen {
		end := off + pieceLen
		if end > int64(len(data)) {
			end = int64(len(data))
		}
		sum := sha1.Sum(data[off:end])
		pieces = append(pieces, sum[:]...)
	}
	info := fmt.Sprintf("d6:lengthi%de4:name5:t.bin12:piece lengthi%de6:pieces%d:%se",
		len(data), pieceLen, len(pieces), string(pieces))
	return []byte("d4:info" + info + "e")
}

type fakeSource struct {
	torrent   []byte
	pieces    map[int][]byte
	rec       ltclient.ResumeRecord
	reads     int
	stopped   bool
	removed   bool
	keptData  bool
	readErrAt int // 1-based piece index that fails; 0 = never
	log       *[]string
}

func (f *fakeSource) ExportState(string) (*ltclient.ResumeRecord, error) {
	r := f.rec
	return &r, nil
}
func (f *fakeSource) GetTorrentFile(string) ([]byte, error) { return f.torrent, nil }
func (f *fakeSource) ReadPiece(_ string, i int) ([]byte, error) {
	f.reads++
	if f.readErrAt > 0 && f.reads == f.readErrAt {
		return nil, fmt.Errorf("simulated read failure")
	}
	return f.pieces[i], nil
}
func (f *fakeSource) StopTorrent(string) error {
	f.stopped = true
	*f.log = append(*f.log, "source.stop")
	return nil
}
func (f *fakeSource) RemoveTorrent(_ string, keepData bool) error {
	f.removed, f.keptData = true, keepData
	*f.log = append(*f.log, "source.remove")
	return nil
}

type fakeSink struct {
	present  map[int]bool
	numPiece int
	adopted  *ltclient.ResumeRecord
	written  map[int][]byte
	verified bool
	started  bool
	savePath string
	log      *[]string
}

func (f *fakeSink) ExportState(string) (*ltclient.ResumeRecord, error) {
	if f.adopted == nil {
		return nil, fmt.Errorf("not held here")
	}
	bits := make([]byte, (f.numPiece+7)/8)
	for i := range f.present {
		bits[i/8] |= 1 << (7 - uint(i%8))
	}
	r := *f.adopted
	r.Bitfield = hex.EncodeToString(bits)
	return &r, nil
}
func (f *fakeSink) ImportStateWithFile(rec *ltclient.ResumeRecord, _ []byte) (string, error) {
	cp := *rec
	f.adopted = &cp
	f.savePath = rec.SavePath
	*f.log = append(*f.log, "sink.adopt")
	return rec.InfoHash, nil
}
func (f *fakeSink) WritePiece(_ string, i int, data []byte) error {
	if f.written == nil {
		f.written = map[int][]byte{}
	}
	f.written[i] = data
	f.present[i] = true
	return nil
}
func (f *fakeSink) VerifyTorrent(string) error {
	f.verified = true
	*f.log = append(*f.log, "sink.verify")
	return nil
}
func (f *fakeSink) StartTorrent(string) error {
	f.started = true
	*f.log = append(*f.log, "sink.start")
	return nil
}

type rig struct {
	src    *fakeSource
	sink   *fakeSink
	runner *RemoteMoveRunner
	job    *store.Job
	log    []string
	pieces int
}

func newRig(t *testing.T, p RemoteMoveParams, free int64) *rig {
	t.Helper()
	data := make([]byte, 5000)
	for i := range data {
		data[i] = byte(i * 3 % 251)
	}
	const pieceLen = 1024
	tor := buildTorrent(data, pieceLen)
	n := (len(data) + pieceLen - 1) / pieceLen

	r := &rig{pieces: n}
	r.src = &fakeSource{
		torrent: tor,
		pieces:  map[int][]byte{},
		rec:     ltclient.ResumeRecord{InfoHash: testIH, SavePath: "/src", SeedMode: true, Bitfield: "ffff"},
		log:     &r.log,
	}
	for i := 0; i < n; i++ {
		end := (i + 1) * pieceLen
		if end > len(data) {
			end = len(data)
		}
		r.src.pieces[i] = data[i*pieceLen : end]
	}
	r.sink = &fakeSink{present: map[int]bool{}, numPiece: n, log: &r.log}

	if p.Category == "" {
		p.Category = "films"
	}
	p.TargetAgent = "heracles"
	blob, _ := json.Marshal(p)
	r.job = &store.Job{ID: "j1", Type: store.JobTypeMoveDataRemote, InfoHash: testIH, Params: string(blob)}

	r.runner = &RemoteMoveRunner{
		DialSource:      func(RemoteMoveParams) (PieceSource, error) { return r.src, nil },
		DialSink:        func(RemoteMoveParams) (PieceSink, error) { return r.sink, nil },
		ResolveSavePath: func(agent, cat string) (string, error) { return "/target/" + cat, nil },
		FreeSpace:       func(string, string) (int64, error) { return free, nil },
	}
	return r
}

func (r *rig) run(ctx context.Context) error {
	return r.runner.Run(ctx, r.job, func(done, total int64) {})
}

// ── tests ───────────────────────────────────────────────────────────────────

func TestDuplicateTransfersEverythingAndKeepsSource(t *testing.T) {
	r := newRig(t, RemoteMoveParams{}, 1<<30)
	if err := r.run(context.Background()); err != nil {
		t.Fatalf("run: %v", err)
	}
	if len(r.sink.written) != r.pieces {
		t.Errorf("wrote %d pieces, want %d", len(r.sink.written), r.pieces)
	}
	if !r.sink.verified || !r.sink.started {
		t.Errorf("target not verified/started: %+v", r.log)
	}
	if r.src.stopped || r.src.removed {
		t.Error("a duplicate must not touch the source")
	}
	if r.sink.savePath != "/target/films" {
		t.Errorf("save_path = %q, want the category's path for the agent", r.sink.savePath)
	}
}

// The adopted record must NOT inherit the source's progress: the target has no
// bytes yet, and a torrent that believes it is complete serves zeros.
func TestAdoptClearsBitfieldAndPauses(t *testing.T) {
	r := newRig(t, RemoteMoveParams{}, 1<<30)
	if err := r.run(context.Background()); err != nil {
		t.Fatalf("run: %v", err)
	}
	if r.sink.adopted == nil {
		t.Fatal("nothing was adopted")
	}
	if r.sink.adopted.Bitfield != "" {
		t.Errorf("adopted with bitfield %q, want empty", r.sink.adopted.Bitfield)
	}
	if !r.sink.adopted.Paused {
		t.Error("adopted un-paused: it would announce while still a shell")
	}
	if r.sink.adopted.SeedMode {
		t.Error("adopted in seed_mode: it would claim completeness it does not have")
	}
}

func TestMoveReleasesSourceLastAndOnlyAfterTargetIsLive(t *testing.T) {
	r := newRig(t, RemoteMoveParams{ReleaseSource: true}, 1<<30)
	if err := r.run(context.Background()); err != nil {
		t.Fatalf("run: %v", err)
	}
	if !r.src.stopped || !r.src.removed {
		t.Fatal("a move must release the source")
	}
	got := strings.Join(r.log, ",")
	want := "sink.adopt,sink.verify,sink.start,source.stop,source.remove"
	if got != want {
		t.Errorf("ordering = %q, want %q", got, want)
	}
}

// The resume point is the target's bitfield: what it already has is not re-sent,
// and an existing torrent is never re-adopted (that would discard the progress).
func TestResumeSkipsPresentPiecesAndDoesNotReAdopt(t *testing.T) {
	r := newRig(t, RemoteMoveParams{}, 1<<30)
	r.sink.adopted = &ltclient.ResumeRecord{InfoHash: testIH, SavePath: "/target/films"}
	r.sink.present[0], r.sink.present[1], r.sink.present[2] = true, true, true

	if err := r.run(context.Background()); err != nil {
		t.Fatalf("run: %v", err)
	}
	if want := r.pieces - 3; r.src.reads != want {
		t.Errorf("read %d pieces, want %d (3 already present)", r.src.reads, want)
	}
	for _, e := range r.log {
		if e == "sink.adopt" {
			t.Fatal("re-adopted on resume: that discards the bitfield")
		}
	}
}

func TestPreflightRefusesBeforeMovingAByte(t *testing.T) {
	r := newRig(t, RemoteMoveParams{}, 10) // 10 bytes free, payload is 5000
	err := r.run(context.Background())
	if err == nil {
		t.Fatal("expected a refusal for lack of space")
	}
	if !strings.Contains(err.Error(), "free") {
		t.Errorf("error should name the space problem, got %v", err)
	}
	if r.src.reads != 0 || len(r.sink.written) != 0 {
		t.Error("preflight ran too late: bytes already moved")
	}
}

// A failure mid-transfer must leave the source as the working copy.
func TestFailedTransferNeverReleasesSource(t *testing.T) {
	r := newRig(t, RemoteMoveParams{ReleaseSource: true}, 1<<30)
	r.src.readErrAt = 2
	if err := r.run(context.Background()); err == nil {
		t.Fatal("expected the read failure to fail the job")
	}
	if r.src.stopped || r.src.removed {
		t.Fatal("source released despite a failed transfer")
	}
	if r.sink.started {
		t.Error("target started despite a failed transfer")
	}
}

func TestCancellationStopsAndSpareSource(t *testing.T) {
	r := newRig(t, RemoteMoveParams{ReleaseSource: true}, 1<<30)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := r.run(ctx); err == nil {
		t.Fatal("expected cancellation to fail the job")
	}
	if r.src.removed {
		t.Error("source removed on a cancelled move")
	}
}

func TestMissingCategoryPathFailsTheJob(t *testing.T) {
	r := newRig(t, RemoteMoveParams{}, 1<<30)
	r.runner.ResolveSavePath = func(string, string) (string, error) {
		return "", fmt.Errorf("category %q defines no save path for agent", "films")
	}
	err := r.run(context.Background())
	if err == nil {
		t.Fatal("expected a refusal when the category has no path for the agent")
	}
	if len(r.sink.written) != 0 {
		t.Error("bytes moved despite an unresolved destination")
	}
}

func TestResumeRejectsUnreadableParams(t *testing.T) {
	r := &RemoteMoveRunner{}
	ok, reason := r.Resume(&store.Job{Params: "{not json"})
	if ok {
		t.Fatal("a job with unreadable params must not be resumed")
	}
	if reason == "" {
		t.Error("a refusal must say why")
	}
}
