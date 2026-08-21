package ltclient

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// Client connects to a hydra-engine Unix socket and provides a typed API.
//
// A dedicated reader goroutine (readLoop) consumes the socket continuously and
// fan-outs frames in two directions: typhon events → SetEventHandler callback,
// RPC responses → per-id waiting channel used by call(). Without this separate
// reader, events would only be drained while some call() happened to be active,
// causing them to batch up between RPCs.
type Client struct {
	socketPath string
	conn       net.Conn
	reader     *bufio.Reader

	writeMu sync.Mutex // serializes writes on the socket
	idSeq   atomic.Int64

	// Bulk lane: a SECOND connection carrying large responses (list_torrents,
	// ~75MB at 100k torrents). Keeping them off the primary connection stops a
	// giant response from head-of-line-blocking the reader and from holding the
	// primary rpcSem — which was timing out small RPCs like get_status and
	// blanking the torrent-detail panel under load.
	bulkConn    net.Conn
	bulkWriteMu sync.Mutex
	bulkSem     chan struct{}

	// pending is indexed by request id. The reader fulfills a call() by
	// delivering the matching response line on ch and removing the entry.
	pendingMu sync.Mutex
	pending   map[int64]chan pendingResp

	// Event handling
	eventMu sync.Mutex
	onEvent func(Event)

	// Shared list_torrents snapshot, gated by the list_cache flag (opt.go).
	listMu       sync.Mutex
	listCache    *ListTorrentsResult
	listCachedAt time.Time
	listLenHint  atomic.Int64
	// The slim projection gets its own slot: handing a caller that asked for
	// the full listing a slim one would silently blank most of its fields.
	slimCache    *ListTorrentsResult
	slimCachedAt time.Time
	slimLenHint  atomic.Int64

	done   chan struct{}
	closed atomic.Bool

	// rpcSem borne le nombre de RPC call() concurrentes. 2.4.10 enleve la
	// serialisation du mutex global et laisse N loops Go tirer sur typhon-engine
	// en parallele, causant contention des locks torrent-level cote Rust. Borner
	// a 8 garde du parallelisme tout en laissant respirer l'engine.
	rpcSem chan struct{}
}

// pendingResp carries either the raw response line or a transport error
// (connection closed while waiting).
type pendingResp struct {
	raw []byte
	err error

	// errMsg is the engine-side error field, already read by the reader while
	// routing. routed says it is trustworthy (the reader ran the single-pass
	// path), letting callVia skip a full re-parse of the frame.
	errMsg string
	routed bool
}

// callTimeout bounds how long a single call() waits for its response.
// Long enough for slow ops (verify, list_torrents on 13k items) while
// still unblocking on engine deadlock.
const callTimeout = 120 * time.Second

// dialTarget maps an engine socket path to a net.Dial (network, address).
// "tcp://host:port" selects the TCP loopback transport (used on Windows/macOS
// where the Go<->engine IPC path has no Unix domain socket); anything else is
// a Unix domain socket path (default, unchanged on Linux).
func dialTarget(socketPath string) (network, address string) {
	if a, ok := strings.CutPrefix(socketPath, "tcp://"); ok {
		return "tcp", a
	}
	return "unix", socketPath
}

// IsTCP reports whether the socket path selects the TCP loopback transport.
func IsTCP(socketPath string) bool { return strings.HasPrefix(socketPath, "tcp://") }

// Connect creates a new client connected to the given engine endpoint and
// starts the background reader.
func Connect(socketPath string) (*Client, error) {
	network, address := dialTarget(socketPath)
	conn, err := net.DialTimeout(network, address, 10*time.Second)
	if err != nil {
		return nil, fmt.Errorf("ltclient: connect %s: %w", socketPath, err)
	}

	// bufio.Reader, not bufio.Scanner: frames are assembled by readFrame below,
	// which can do it in one pass. Scanner re-scanned the whole accumulated
	// token on every refill — quadratic on the ~100MB list_torrents frame.
	// The 1MB buffer is the read chunk, no longer a cap on frame size.
	reader := bufio.NewReaderSize(conn, 1024*1024)

	// Second connection for the bulk lane (list_torrents). The engine's RPC
	// server accepts multiple connections, each served independently.
	bulkConn, berr := net.DialTimeout(network, address, 10*time.Second)
	if berr != nil {
		conn.Close()
		return nil, fmt.Errorf("ltclient: connect(bulk) %s: %w", socketPath, berr)
	}
	bulkReader := bufio.NewReaderSize(bulkConn, 1024*1024)

	c := &Client{
		socketPath: socketPath,
		conn:       conn,
		reader:     reader,
		bulkConn:   bulkConn,
		bulkSem:    make(chan struct{}, 4),
		pending:    make(map[int64]chan pendingResp),
		done:       make(chan struct{}),
		rpcSem:     make(chan struct{}, 8),
	}

	go c.readLoop(c.reader, true)
	go c.readLoop(bulkReader, false)
	return c, nil
}

// SetEventHandler sets a callback for events pushed by the engine.
func (c *Client) SetEventHandler(handler func(Event)) {
	c.eventMu.Lock()
	c.onEvent = handler
	c.eventMu.Unlock()
}

// Close closes the connection. The reader goroutine exits and all pending
// calls fail with a "connection closed" error.
func (c *Client) Close() error {
	if c.closed.Swap(true) {
		return nil
	}
	err := c.conn.Close()
	if c.bulkConn != nil {
		c.bulkConn.Close()
	}
	close(c.done)
	return err
}

// readLoop is the single consumer of the socket. It dispatches events to the
// registered handler and routes RPC responses to the per-id channel. Exits on
// connection close or unrecoverable read error, draining all pending calls.
func (c *Client) readLoop(r *bufio.Reader, drainOnExit bool) {
	if drainOnExit {
		defer c.drainPending(errors.New("ltclient: connection closed"))
	}

	for {
		buf, err := readFrame(r)
		if err != nil {
			if !c.closed.Load() && err != io.EOF {
				slog.Warn("ltclient: read loop exiting", "error", err)
			}
			return
		}
		if len(buf) == 0 {
			continue
		}

		// Route the frame. head.Event/ID/Error is all routing needs; the
		// caller decodes the payload itself from the same bytes.
		head, err := routeFrame(buf)
		if err != nil {
			slog.Warn("ltclient: bad JSON from engine", "error", err)
			continue
		}

		if head.Event != "" {
			var ev Event
			if err := json.Unmarshal(buf, &ev); err != nil {
				continue
			}
			c.eventMu.Lock()
			handler := c.onEvent
			c.eventMu.Unlock()
			if handler != nil {
				// Don't block the reader — handlers may do heavy work
				// (json unmarshal into 13k-entry maps, mutex contention).
				go handler(ev)
			}
			continue
		}

		c.pendingMu.Lock()
		ch, ok := c.pending[head.ID]
		if ok {
			delete(c.pending, head.ID)
		}
		c.pendingMu.Unlock()
		if !ok {
			slog.Warn("ltclient: orphan response", "id", head.ID)
			continue
		}
		// ch is buffered (cap 1) — non-blocking. errMsg/routed let callVia skip
		// re-parsing the whole frame just to read back the error field.
		ch <- pendingResp{raw: buf, errMsg: head.Error, routed: optRoute.Load()}
	}
}

// frameHead is everything routing needs from a frame. It deliberately omits
// result/data: encoding/json then SKIPS those values instead of allocating a
// RawMessage copy of them, which on a 100MB list_torrents frame is the whole
// cost. See opt.go for why this is gated.
type frameHead struct {
	ID    int64  `json:"id"`
	Error string `json:"error"`
	Event string `json:"event"`
}

// routeFrame extracts the routing header from a frame.
func routeFrame(buf []byte) (frameHead, error) {
	var head frameHead
	if optRoute.Load() {
		// One pass, values skipped.
		return head, json.Unmarshal(buf, &head)
	}

	// Baseline path: the pre-3.42.4 behaviour, kept so the OFF rung of a
	// measurement ladder is a real baseline. Parses the frame twice — once to
	// test for "event", once into the full Response (which also allocates a
	// copy of result/data).
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(buf, &raw); err != nil {
		return head, err
	}
	if _, hasEvent := raw["event"]; hasEvent {
		head.Event = "event"
		return head, nil
	}
	var resp Response
	if err := json.Unmarshal(buf, &resp); err != nil {
		return head, err
	}
	head.ID, head.Error = resp.ID, resp.Error
	return head, nil
}

// readFrame reads one newline-terminated frame and returns it. The returned
// slice is owned by the caller (no shared buffer to copy out of, unlike the
// bufio.Scanner this replaced).
// frameHint remembers the largest frame seen, so the next big one can be
// allocated once instead of climbing the append doubling cascade. Guarded by
// optPrealloc; see opt.go.
var frameHint atomic.Int64

func readFrame(r *bufio.Reader) ([]byte, error) {
	var buf []byte
	for {
		chunk, err := r.ReadSlice('\n')
		if err == bufio.ErrBufferFull {
			// First overflow: this frame does not fit the bufio buffer, so it
			// is one of the big ones (list_torrents). Size it in one shot from
			// the high-water mark instead of doubling our way there. Done HERE
			// and not on entry: small frames return on the first ReadSlice and
			// must not pay for a 100MB allocation.
			if buf == nil && optPrealloc.Load() {
				if h := frameHint.Load(); h > int64(len(chunk)) {
					buf = make([]byte, 0, h)
				}
			}
			buf = append(buf, chunk...)
			if !optFrame.Load() {
				// Baseline path: bufio.Scanner re-scanned the ENTIRE
				// accumulated token for the delimiter on every refill. That
				// rescan is the quadratic cost we are measuring against; the
				// result is discarded because ReadSlice already told us the
				// delimiter is not in this chunk.
				_ = bytes.IndexByte(buf, '\n')
			}
			continue
		}
		if err != nil {
			if len(buf) > 0 && err == io.EOF {
				// Trailing bytes without a newline: incomplete frame, drop it.
				return nil, io.EOF
			}
			return nil, err
		}
		// Drop the trailing '\n' to match the old Scanner contract.
		chunk = chunk[:len(chunk)-1]
		if buf == nil {
			out := make([]byte, len(chunk))
			copy(out, chunk)
			return out, nil
		}
		out := append(buf, chunk...)
		if optPrealloc.Load() {
			// An eighth of headroom so a frame that grows slightly does not
			// immediately force a reallocation.
			if want := int64(len(out) + len(out)/8); want > frameHint.Load() {
				frameHint.Store(want)
			}
		}
		return out, nil
	}
}

// drainPending fails every outstanding call with err. Called when the reader
// exits so that in-flight callers don't hang.
func (c *Client) drainPending(err error) {
	c.pendingMu.Lock()
	defer c.pendingMu.Unlock()
	for id, ch := range c.pending {
		ch <- pendingResp{err: err}
		delete(c.pending, id)
	}
}

// call sends a JSON-RPC request and waits for the matching response.
//
// Register-before-write is important: if we wrote first and the engine replied
// faster than this goroutine could insert into `pending`, the reader would
// discard the response as orphan.
func (c *Client) call(method string, params interface{}) (json.RawMessage, error) {
	return c.callVia(c.conn, &c.writeMu, c.rpcSem, method, params)
}

// callBulk routes over the dedicated bulk connection (large responses like
// list_torrents), so they never contend with small RPCs on the primary lane.
//
// listLenHint carries the previous decode's torrent count so the next one can
// size its slice in a single allocation. See ListTorrents.
func (c *Client) callBulk(method string, params interface{}) (json.RawMessage, error) {
	return c.callVia(c.bulkConn, &c.bulkWriteMu, c.bulkSem, method, params)
}

func (c *Client) callVia(conn net.Conn, wmu *sync.Mutex, sem chan struct{}, method string, params interface{}) (json.RawMessage, error) {
	if c.closed.Load() {
		return nil, errors.New("ltclient: client closed")
	}

	// Patch 2: borne les RPC concurrentes pour ne pas saturer typhon-engine.
	// Timeout court si bloque : eviter que tout se bloque derriere 8 RPC lentes.
	select {
	case sem <- struct{}{}:
		defer func() { <-sem }()
	case <-time.After(5 * time.Second):
		return nil, errors.New("ltclient: rpc semaphore acquire timeout (engine saturated?)")
	}

	id := c.idSeq.Add(1)

	paramsJSON, err := json.Marshal(params)
	if err != nil {
		return nil, fmt.Errorf("ltclient: marshal params: %w", err)
	}

	req := Request{
		ID:     id,
		Method: method,
		Params: paramsJSON,
	}

	reqBytes, err := json.Marshal(req)
	if err != nil {
		return nil, fmt.Errorf("ltclient: marshal request: %w", err)
	}
	reqBytes = append(reqBytes, '\n')

	ch := make(chan pendingResp, 1)
	c.pendingMu.Lock()
	c.pending[id] = ch
	c.pendingMu.Unlock()

	wmu.Lock()
	_, werr := conn.Write(reqBytes)
	wmu.Unlock()
	if werr != nil {
		c.pendingMu.Lock()
		delete(c.pending, id)
		c.pendingMu.Unlock()
		return nil, fmt.Errorf("ltclient: write: %w", werr)
	}

	select {
	case pr := <-ch:
		if pr.err != nil {
			return nil, pr.err
		}
		if pr.routed {
			// The reader already extracted the error field in its single pass.
			if pr.errMsg != "" {
				return nil, fmt.Errorf("ltclient: engine error: %s", pr.errMsg)
			}
		} else {
			// Baseline path: parse the whole frame a third time for one string.
			var resp Response
			if err := json.Unmarshal(pr.raw, &resp); err != nil {
				return nil, fmt.Errorf("ltclient: unmarshal response: %w", err)
			}
			if resp.Error != "" {
				return nil, fmt.Errorf("ltclient: engine error: %s", resp.Error)
			}
		}
		// Legacy contract: callers unmarshal the full line directly into
		// their own result struct (unknown fields like "id" are ignored).
		return pr.raw, nil
	case <-time.After(callTimeout):
		c.pendingMu.Lock()
		delete(c.pending, id)
		c.pendingMu.Unlock()
		return nil, fmt.Errorf("ltclient: timeout waiting for response id=%d method=%s", id, method)
	}
}

// Ping checks connectivity.
func (c *Client) Ping() error {
	_, err := c.call("ping", map[string]interface{}{})
	return err
}

// AddTorrent adds a torrent from a .torrent file.
func (c *Client) AddTorrent(torrentPath, savePath string, stopped bool) (*AddTorrentResult, error) {
	return c.AddTorrentWithOptions(torrentPath, savePath, stopped, false)
}

// AddTorrentWithOptions adds a torrent with seed_mode support.
// seed_mode=true skips verification (trust data is complete).
func (c *Client) AddTorrentWithOptions(torrentPath, savePath string, stopped, seedMode bool) (*AddTorrentResult, error) {
	raw, err := c.call("add_torrent", AddTorrentParams{
		TorrentPath: torrentPath,
		SavePath:    savePath,
		Stopped:     stopped,
		SeedMode:    seedMode,
	})
	if err != nil {
		return nil, err
	}
	var result AddTorrentResult
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal add_torrent: %w", err)
	}
	if result.Error != "" {
		return nil, fmt.Errorf("ltclient: %s", result.Error)
	}
	return &result, nil
}

// FetchMetadata asks the engine to start resolving a magnet. It returns as soon
// as the job is queued -- resolution takes seconds to minutes, so the engine
// runs it in the background and we poll GetMetadata.
func (c *Client) FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*FetchMetadataResult, error) {
	raw, err := c.call("fetch_metadata", FetchMetadataParams{
		InfoHash:  infoHash,
		Trackers:  trackers,
		Peers:     peers,
		BindingID: bindingID,
	})
	if err != nil {
		return nil, err
	}
	var result FetchMetadataResult
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal fetch_metadata: %w", err)
	}
	if result.Error != "" {
		return nil, fmt.Errorf("ltclient: %s", result.Error)
	}
	return &result, nil
}

// GetMetadata polls a resolution job. On "done" the engine hands over the raw
// info dict and forgets the job, so a successful poll only ever fires once.
func (c *Client) GetMetadata(infoHash string) (*GetMetadataResult, error) {
	raw, err := c.call("get_metadata", map[string]string{"info_hash": infoHash})
	if err != nil {
		return nil, err
	}
	var result GetMetadataResult
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal get_metadata: %w", err)
	}
	return &result, nil
}

// RemoveTorrent removes a torrent. If keepData is true, files are kept.
func (c *Client) RemoveTorrent(infoHash string, keepData bool) error {
	_, err := c.call("remove_torrent", map[string]interface{}{
		"info_hash": infoHash,
		"keep_data": keepData,
	})
	return err
}

// StartTorrent resumes a paused torrent.
func (c *Client) StartTorrent(infoHash string) error {
	_, err := c.call("start_torrent", map[string]interface{}{
		"info_hash": infoHash,
	})
	return err
}

// StopTorrent pauses a torrent.
func (c *Client) StopTorrent(infoHash string) error {
	_, err := c.call("stop_torrent", map[string]interface{}{
		"info_hash": infoHash,
	})
	return err
}

// SetServingSuspended toggles disk-serving suspension (HDD quiet-mode lever):
// the torrent keeps peers + announces but serves no Requests (zero disk I/O).
func (c *Client) SetServingSuspended(infoHash string, suspended bool) error {
	_, err := c.call("set_serving_suspended", map[string]interface{}{
		"info_hash": infoHash,
		"suspended": suspended,
	})
	return err
}

// SetSavePath swaps the in-memory save_path for a running torrent and
// flushes fastresume. Caller must have stopped the torrent and moved
// the files on disk before invoking this.
func (c *Client) SetSavePath(infoHash, savePath string) error {
	_, err := c.call("set_save_path", map[string]interface{}{
		"info_hash": infoHash,
		"save_path": savePath,
	})
	return err
}

// VerifyTorrent forces a data integrity re-check.
func (c *Client) VerifyTorrent(infoHash string) error {
	_, err := c.call("verify_torrent", map[string]interface{}{
		"info_hash": infoHash,
	})
	return err
}

// GetStatus returns detailed status for a single torrent.
func (c *Client) GetStatus(infoHash string) (*TorrentStatus, error) {
	raw, err := c.call("get_status", map[string]interface{}{
		"info_hash": infoHash,
	})
	if err != nil {
		return nil, err
	}
	var status TorrentStatus
	if err := json.Unmarshal(raw, &status); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal status: %w", err)
	}
	return &status, nil
}

// SubscribeEvents opts into push-based events from the engine.
// After this call the engine will emit Event frames ({"event":"...","data":{...}})
// on the same socket (alongside regular request/reply pairs). The reader
// goroutine dispatches them to the SetEventHandler callback as they arrive.
//
// Typhon emits at least: torrent_added, torrent_removed, stats_snapshot.
// See typhon-engine/src/rpc/events.rs for the full event set.
//
// Called once per Client lifetime (after SetEventHandler).
func (c *Client) SubscribeEvents() error {
	_, err := c.call("subscribe_events", map[string]interface{}{})
	return err
}

// ListTorrents returns all torrents in the session.
// ListTorrents returns every field the engine knows about each torrent.
func (c *Client) ListTorrents() (*ListTorrentsResult, error) { return c.listTorrents(false) }

// ListTorrentsSlim returns only the eight fields the scheduling loops read:
// info hash, state, progress, total size, bytes done, download rate, paused and
// finished. Everything else on TorrentStatus comes back zero, so this is for
// callers that have been checked against that list -- not a drop-in for
// ListTorrents.
//
// The full listing is the largest thing either process allocates at 196k
// torrents, and the three loops that poll it every 10-30s read a handful of
// fields. The engine skips the strings and the per-torrent mutexes it would
// need for the rest.
func (c *Client) ListTorrentsSlim() (*ListTorrentsResult, error) { return c.listTorrents(true) }

func (c *Client) listTorrents(slim bool) (*ListTorrentsResult, error) {
	if optListCache.Load() {
		if r := c.cachedListFor(slim); r != nil {
			return r, nil
		}
	}

	params := map[string]interface{}{}
	if slim {
		params["slim"] = true
	}
	raw, err := c.callBulk("list_torrents", params)
	if err != nil {
		return nil, err
	}
	// encoding/json grows a slice it decodes into by repeated doubling, so a
	// 196k-element list throws away roughly the whole array again in dead
	// intermediates every single decode (measured: 2.2GB per 90s, 25% of all
	// allocation in the process). Handing it a slice already sized from the
	// previous decode makes it fill that one instead of growing its own. The
	// count only moves when torrents are added or removed, so the hint is
	// right nearly always and merely imperfect when it is not.
	lenHint := &c.listLenHint
	if slim {
		lenHint = &c.slimLenHint
	}
	result := ListTorrentsResult{}
	if hint := int(lenHint.Load()); hint > 0 {
		result.Torrents = make([]TorrentStatus, 0, hint)
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal list: %w", err)
	}
	lenHint.Store(int64(len(result.Torrents)))

	if optListCache.Load() {
		c.listMu.Lock()
		if slim {
			c.slimCache = &result
			c.slimCachedAt = time.Now()
		} else {
			c.listCache = &result
			c.listCachedAt = time.Now()
		}
		c.listMu.Unlock()
	}
	return &result, nil
}

// cachedList returns a private copy of the shared snapshot if it is fresh
// enough, else nil. Three schedulers (reconcile, enforceDownloadSlots,
// refreshStats) each pulled and decoded their own copy of all 107k torrents;
// this lets them share one pull.
//
// The copy is not optional: enforceDownloadSlots sorts what it gets, so handing
// out the same backing array would let one caller reorder another's view mid-
// scan. Copying a slice of structs is a memmove — cheap next to decoding 100MB
// of JSON, which is the whole point.
// cachedList returns the shared snapshot itself, not a copy.
//
// THE RESULT IS READ-ONLY. Every caller ranges over it and reads; none sorts,
// appends to or writes through it, and a refresh replaces the whole
// *ListTorrentsResult rather than mutating it, so a caller still holding the
// previous one keeps a consistent view. Copying instead cost a 196k-element
// array on every cache hit -- the copy was pure overhead, since what it
// protected against does not happen. A future caller that needs to mutate must
// copy what it needs, not silently write here.
func (c *Client) cachedList() *ListTorrentsResult { return c.cachedListFor(false) }

func (c *Client) cachedListFor(slim bool) *ListTorrentsResult {
	c.listMu.Lock()
	defer c.listMu.Unlock()
	cached, at := c.listCache, c.listCachedAt
	if slim {
		cached, at = c.slimCache, c.slimCachedAt
	}
	if cached == nil || time.Since(at).Nanoseconds() > listCacheTTL.Load() {
		return nil
	}
	return cached
}

// GetPeers returns connected peers for a torrent.
func (c *Client) GetPeers(infoHash string) ([]PeerInfo, error) {
	raw, err := c.call("get_peers", map[string]interface{}{
		"info_hash": infoHash,
	})
	if err != nil {
		return nil, err
	}
	var result GetPeersResult
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal peers: %w", err)
	}
	return result.Peers, nil
}

// AddPeers injects peers into a torrent.
func (c *Client) AddPeers(infoHash string, peers []struct {
	IP   string
	Port int
}) error {
	peerList := make([]map[string]interface{}, len(peers))
	for i, p := range peers {
		peerList[i] = map[string]interface{}{"ip": p.IP, "port": p.Port}
	}
	_, err := c.call("add_peers", map[string]interface{}{
		"info_hash": infoHash,
		"peers":     peerList,
	})
	return err
}

// SetDialsPaused holds or releases the engine's outbound dial queue. Used by
// the startup pause: the Go side stops announcing, this stops the engine from
// dialing anything it already knows about (PEX, DHT, peers from earlier
// announces), so between the two nothing outbound leaves the process.
func (c *Client) SetDialsPaused(paused bool) error {
	_, err := c.call("set_dials_paused", map[string]interface{}{
		"paused": paused,
	})
	return err
}

// SetUploadLimit sets the session upload rate limit in bytes/s (0 = unlimited).
func (c *Client) SetUploadLimit(limitBytes int) error {
	_, err := c.call("set_upload_limit", map[string]interface{}{
		"limit_bytes": limitBytes,
	})
	return err
}

// SetDownloadLimit sets the session download rate limit in bytes/s (0 = unlimited).
func (c *Client) SetDownloadLimit(limitBytes int) error {
	_, err := c.call("set_download_limit", map[string]interface{}{
		"limit_bytes": limitBytes,
	})
	return err
}

// SetListenPort hot-rebinds the engine TCP peer listener to a new port with
// no restart (torrents + live peer connections kept). Used when a dynamic
// upstream port rotates (gluetun / Proton port-forward).
func (c *Client) SetListenPort(port int) error {
	_, err := c.call("set_listen_port", map[string]interface{}{
		"port": port,
	})
	return err
}

// SetSelfIPs replaces the engine self-dial IP filter with the given public IPs
// (dynamic — avoids a wasted self-connect when our ISP lease changes).
func (c *Client) SetSelfIPs(ips []string) error {
	_, err := c.call("set_self_ips", map[string]interface{}{
		"ips": ips,
	})
	return err
}

// GetSessionStats returns aggregate session statistics.
func (c *Client) GetSessionStats() (*SessionStats, error) {
	raw, err := c.call("get_session_stats", map[string]interface{}{})
	if err != nil {
		return nil, err
	}
	var stats SessionStats
	if err := json.Unmarshal(raw, &stats); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal session_stats: %w", err)
	}
	return &stats, nil
}

// GetFiles returns the file list for a torrent.
func (c *Client) GetFiles(infoHash string) ([]FileInfo, error) {
	raw, err := c.call("get_files", map[string]interface{}{
		"info_hash": infoHash,
	})
	if err != nil {
		return nil, err
	}
	var result GetFilesResult
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal files: %w", err)
	}
	return result.Files, nil
}

// SetEngineOptFlag toggles one engine-side optimisation at runtime. Value is
// only read by the flags that carry a number rather than a boolean.
func (c *Client) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	raw, err := c.call("set_opt_flag", map[string]interface{}{
		"flag":  name,
		"on":    on,
		"value": value,
	})
	if err != nil {
		return nil, err
	}
	var result struct {
		Flags map[string]interface{} `json:"flags"`
		Error string                 `json:"error"`
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal set_opt_flag: %w", err)
	}
	if result.Error != "" {
		return nil, fmt.Errorf("engine: %s", result.Error)
	}
	return result.Flags, nil
}

// EngineOptFlags reports the engine-side flag state.
func (c *Client) EngineOptFlags() (map[string]interface{}, error) {
	raw, err := c.call("get_opt_flags", map[string]interface{}{})
	if err != nil {
		return nil, err
	}
	var result struct {
		Flags map[string]interface{} `json:"flags"`
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal get_opt_flags: %w", err)
	}
	return result.Flags, nil
}

// GetAvailability returns swarm piece availability for a torrent.
func (c *Client) GetAvailability(infoHash string) (*Availability, error) {
	raw, err := c.call("get_availability", map[string]interface{}{
		"info_hash": infoHash,
	})
	if err != nil {
		return nil, err
	}
	var result Availability
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal availability: %w", err)
	}
	return &result, nil
}

// TrackerInfo represents a single tracker entry.
type TrackerInfo struct {
	URL       string          `json:"url"`
	Tier      int             `json:"tier"`
	Verified  bool            `json:"verified"`
	Endpoints json.RawMessage `json:"endpoints"`
}

// GetTrackers returns tracker info for a torrent.
func (c *Client) GetTrackers(infoHash string) ([]TrackerInfo, error) {
	raw, err := c.call("get_trackers", map[string]interface{}{"info_hash": infoHash})
	if err != nil {
		return nil, err
	}
	var result struct {
		Trackers []TrackerInfo `json:"trackers"`
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, err
	}
	return result.Trackers, nil
}

// SetTrackers replaces a torrent's tracker list, in tiers, and returns the
// list the engine actually kept. Whole-list replacement is deliberate: add,
// remove and edit are read-modify-write above this layer, so only one call can
// change what gets announced and applying it twice changes nothing.
func (c *Client) SetTrackers(infoHash string, tiers [][]string) ([][]string, error) {
	if tiers == nil {
		// json.Marshal turns a nil slice into null, which the engine rejects as
		// "not an array". An empty list is a legitimate request (announce to
		// nobody), so make it explicit rather than accidental.
		tiers = [][]string{}
	}
	raw, err := c.call("set_trackers", map[string]interface{}{
		"info_hash": infoHash,
		"trackers":  tiers,
	})
	if err != nil {
		return nil, err
	}
	var result struct {
		Trackers [][]string `json:"trackers"`
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, err
	}
	return result.Trackers, nil
}

// GetDiagnostics returns deep libtorrent session diagnostics.
func (c *Client) GetDiagnostics() (*DiagnosticStats, error) {
	raw, err := c.call("get_diagnostics", map[string]interface{}{})
	if err != nil {
		return nil, err
	}
	var stats DiagnosticStats
	if err := json.Unmarshal(raw, &stats); err != nil {
		return nil, fmt.Errorf("ltclient: unmarshal diagnostics: %w", err)
	}
	return &stats, nil
}
