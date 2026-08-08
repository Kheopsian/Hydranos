package api

import (
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/cookiejar"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gin-gonic/gin"
)

// ---------------------------------------------------------------------------
// qBittorrent -> Hydra migration (onboarding import).
//
// Point Hydra at a running qBittorrent WebUI: it logs in, lists the torrents,
// recreates the categories as HOARD-mode categories (so a cold library never
// lands in the race engine), then re-adds every torrent seeding the data
// already on disk. Completed torrents go in seed-mode (skip hash-check, straight
// to seed); partial ones are added normally (verify what is there, download the
// rest). Nothing already complete is re-downloaded -> no recheck CPU storm.
// ---------------------------------------------------------------------------

// ---- qBit WebUI client -----------------------------------------------------

type qbitClient struct {
	base string
	hc   *http.Client
}

func newQbitClient(rawurl string) (*qbitClient, error) {
	rawurl = strings.TrimRight(strings.TrimSpace(rawurl), "/")
	if rawurl == "" {
		return nil, fmt.Errorf("empty qBittorrent URL")
	}
	if !strings.HasPrefix(rawurl, "http://") && !strings.HasPrefix(rawurl, "https://") {
		rawurl = "http://" + rawurl
	}
	if _, err := url.Parse(rawurl); err != nil {
		return nil, fmt.Errorf("invalid URL: %w", err)
	}
	jar, _ := cookiejar.New(nil)
	return &qbitClient{base: rawurl, hc: &http.Client{Timeout: 60 * time.Second, Jar: jar}}, nil
}

func (q *qbitClient) login(user, pass string) error {
	form := url.Values{"username": {user}, "password": {pass}}
	req, _ := http.NewRequest("POST", q.base+"/api/v2/auth/login", strings.NewReader(form.Encode()))
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Referer", q.base)
	resp, err := q.hc.Do(req)
	if err != nil {
		return fmt.Errorf("cannot reach qBittorrent at %s: %w", q.base, err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1024))
	if resp.StatusCode == http.StatusForbidden {
		return fmt.Errorf("qBittorrent refused the login (banned IP after too many attempts?)")
	}
	// qBit returns "Ok." on success, "Fails." on bad creds. LAN-bypass setups
	// return "Ok." even with empty creds.
	if strings.Contains(string(body), "Fails") {
		return fmt.Errorf("invalid qBittorrent username/password")
	}
	// Accept any 2xx/3xx: some setups (WebUI auth bypassed by IP/subnet, or a
	// reverse proxy in front) answer 204 No Content or a redirect on an already
	// valid session. Only a 4xx/5xx is a real failure. torrents/info right after
	// is the real auth gate.
	if resp.StatusCode >= 400 {
		return fmt.Errorf("qBittorrent login failed (HTTP %d)", resp.StatusCode)
	}
	return nil
}

type qbitTorrent struct {
	Hash        string  `json:"hash"`
	Name        string  `json:"name"`
	SavePath    string  `json:"save_path"`
	ContentPath string  `json:"content_path"`
	Category    string  `json:"category"`
	Progress    float64 `json:"progress"`
	State       string  `json:"state"`
	Size        int64   `json:"size"`
	Uploaded    int64   `json:"uploaded"`
	Downloaded  int64   `json:"downloaded"`
	AddedOn     int64   `json:"added_on"`
}

func (q *qbitClient) torrentsInfo() ([]qbitTorrent, error) {
	resp, err := q.hc.Get(q.base + "/api/v2/torrents/info")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusForbidden {
		return nil, fmt.Errorf("qBittorrent rejected the session (bad credentials, or WebUI auth bypass is off)")
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("torrents/info HTTP %d", resp.StatusCode)
	}
	var out []qbitTorrent
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return nil, err
	}
	return out, nil
}

type qbitCategory struct {
	Name     string `json:"name"`
	SavePath string `json:"savePath"`
}

func (q *qbitClient) categories() (map[string]qbitCategory, error) {
	resp, err := q.hc.Get(q.base + "/api/v2/torrents/categories")
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("torrents/categories HTTP %d", resp.StatusCode)
	}
	out := map[string]qbitCategory{}
	// qBit returns {} when there are no categories; tolerate an empty body.
	_ = json.NewDecoder(resp.Body).Decode(&out)
	return out, nil
}

func (q *qbitClient) exportTorrent(hash string) ([]byte, error) {
	resp, err := q.hc.Get(q.base + "/api/v2/torrents/export?hash=" + url.QueryEscape(hash))
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("export HTTP %d", resp.StatusCode)
	}
	return io.ReadAll(io.LimitReader(resp.Body, 64<<20))
}

// ---- helpers ---------------------------------------------------------------

// remapPath rewrites p by the longest matching prefix in pathMap (qBit's on-disk
// layout -> the path Hydra sees inside its container). No match = unchanged.
// importedAsStopped decides whether a torrent should land stopped, from the
// state its previous client reported.
//
// Only a deliberate halt carries over. A torrent the OTHER client's scheduler
// was holding back (queued*) is imported running: that was their queue's
// decision, not a human's, and importing it as stopped would leave it stopped
// forever with nobody to undo it. Same for errored torrents -- our engine will
// form its own opinion.
func importedAsStopped(state string) bool {
	switch state {
	case "pausedUP", "pausedDL", // qBittorrent 4.x
		"stoppedUP", "stoppedDL": // qBittorrent 5.x
		return true
	}
	return false
}

func remapPath(p string, pathMap map[string]string) string {
	best := ""
	for from := range pathMap {
		if from != "" && strings.HasPrefix(p, from) && len(from) > len(best) {
			best = from
		}
	}
	if best == "" {
		return p
	}
	return pathMap[best] + strings.TrimPrefix(p, best)
}

func qbitCatSavePath(cats map[string]qbitCategory, name string) string {
	if c, ok := cats[name]; ok {
		return c.SavePath
	}
	return ""
}

func firstNonEmpty(a, b string) string {
	if a != "" {
		return a
	}
	return b
}

// ---- provenance ------------------------------------------------------------

type provenance struct {
	SourceClient         string `json:"source_client"`
	SourceDate           int64  `json:"source_date"`
	CarriedUploadedBytes int64  `json:"carried_uploaded_bytes"`
	ImportedCount        int    `json:"imported_count"`
}

func provenanceFile(dataDir string) string { return filepath.Join(dataDir, "provenance.json") }

func loadProvenance(dataDir string) (provenance, bool) {
	var p provenance
	data, err := os.ReadFile(provenanceFile(dataDir))
	if err != nil {
		return p, false
	}
	if json.Unmarshal(data, &p) != nil || p.SourceClient == "" {
		return p, false
	}
	return p, true
}

func saveProvenance(dataDir string, p provenance) error {
	data, _ := json.MarshalIndent(p, "", "  ")
	tmp := provenanceFile(dataDir) + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, provenanceFile(dataDir))
}

// ---- import job ------------------------------------------------------------

type importSnapshot struct {
	JobID       string   `json:"job_id"`
	Phase       string   `json:"phase"` // connect|categories|torrents|done|error
	Total       int      `json:"total"`
	Done        int      `json:"done"`
	Seeded      int      `json:"seeded"`
	Downloading int      `json:"downloading"`
	Failed      int      `json:"failed"`
	Skipped     int      `json:"skipped"`
	Current     string   `json:"current"`
	Error       string   `json:"error,omitempty"`
	Errors      []string `json:"errors,omitempty"`
	Finished    bool     `json:"finished"`
}

type importJob struct {
	mu   sync.Mutex
	snap importSnapshot
	subs map[int]chan importSnapshot
	next int
	done chan struct{}
}

func newImportJob(id string) *importJob {
	return &importJob{
		snap: importSnapshot{JobID: id, Phase: "connect"},
		subs: map[int]chan importSnapshot{},
		done: make(chan struct{}),
	}
}

func (j *importJob) get() importSnapshot {
	j.mu.Lock()
	defer j.mu.Unlock()
	return j.snap
}

func (j *importJob) broadcastLocked() {
	for _, ch := range j.subs {
		select {
		case ch <- j.snap:
		default:
		}
	}
}

func (j *importJob) update(fn func(*importSnapshot)) {
	j.mu.Lock()
	fn(&j.snap)
	j.broadcastLocked()
	j.mu.Unlock()
}

func (j *importJob) subscribe() (int, chan importSnapshot, importSnapshot) {
	j.mu.Lock()
	defer j.mu.Unlock()
	id := j.next
	j.next++
	ch := make(chan importSnapshot, 16)
	j.subs[id] = ch
	return id, ch, j.snap
}

func (j *importJob) unsubscribe(id int) {
	j.mu.Lock()
	delete(j.subs, id)
	j.mu.Unlock()
}

func (j *importJob) finish() {
	j.mu.Lock()
	if !j.snap.Finished {
		j.snap.Finished = true
		close(j.done)
	}
	j.broadcastLocked()
	j.mu.Unlock()
}

func appendErr(sn *importSnapshot, msg string) {
	if len(sn.Errors) < 50 {
		sn.Errors = append(sn.Errors, msg)
	}
}

// One import at a time per process (monolith front). Guarded singleton.
var (
	importMu      sync.Mutex
	importCurrent *importJob
)

// ---- HTTP handlers ---------------------------------------------------------

type qbitCreds struct {
	URL      string            `json:"url"`
	Username string            `json:"username"`
	Password string            `json:"password"`
	PathMap  map[string]string `json:"path_map"`
}

// handleQbitPreview is a dry-run: it connects to qBittorrent and reports what
// WOULD be imported (counts, categories, path prefixes, a sample). No writes.
func (s *Server) handleQbitPreview(c *gin.Context) {
	var req qbitCreds
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	cl, err := newQbitClient(req.URL)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if err := cl.login(req.Username, req.Password); err != nil {
		c.JSON(http.StatusBadGateway, gin.H{"error": err.Error()})
		return
	}
	ts, err := cl.torrentsInfo()
	if err != nil {
		c.JSON(http.StatusBadGateway, gin.H{"error": err.Error()})
		return
	}
	cats, _ := cl.categories()

	completed := 0
	var carried int64
	prefixes := map[string]struct{}{}
	catSet := map[string]string{}
	sample := []gin.H{}
	for i, t := range ts {
		if t.Progress >= 1.0 {
			completed++
		}
		carried += t.Uploaded
		if t.SavePath != "" {
			prefixes[t.SavePath] = struct{}{}
		}
		cn := t.Category
		if cn == "" {
			cn = "imported"
		}
		catSet[cn] = remapPath(firstNonEmpty(qbitCatSavePath(cats, t.Category), t.SavePath), req.PathMap)
		if i < 12 {
			sample = append(sample, gin.H{"name": t.Name, "category": cn, "save_path": t.SavePath, "progress": t.Progress})
		}
	}
	catList := []gin.H{}
	for name, sp := range catSet {
		catList = append(catList, gin.H{"name": name, "save_path": sp})
	}
	sort.Slice(catList, func(i, j int) bool { return catList[i]["name"].(string) < catList[j]["name"].(string) })
	prefixList := []string{}
	for p := range prefixes {
		prefixList = append(prefixList, p)
	}
	sort.Strings(prefixList)

	c.JSON(http.StatusOK, gin.H{
		"total":                  len(ts),
		"completed":              completed,
		"incomplete":             len(ts) - completed,
		"carried_uploaded_bytes": carried,
		"categories":             catList,
		"path_prefixes":          prefixList,
		"sample":                 sample,
	})
}

// handleQbitStart kicks off the import in the background and returns a job id.
func (s *Server) handleQbitStart(c *gin.Context) {
	var req qbitCreds
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	importMu.Lock()
	if importCurrent != nil && !importCurrent.get().Finished {
		importMu.Unlock()
		c.JSON(http.StatusConflict, gin.H{"error": "an import is already running"})
		return
	}
	job := newImportJob(fmt.Sprintf("imp-%d", time.Now().UnixNano()))
	importCurrent = job
	importMu.Unlock()

	slog.Info("qbit import: job accepted", "job", job.get().JobID, "url", req.URL)
	go s.runQbitImport(job, req)
	c.JSON(http.StatusOK, gin.H{"job_id": job.get().JobID})
}

func (s *Server) runQbitImport(job *importJob, req qbitCreds) {
	defer job.finish()
	fail := func(msg string) {
		slog.Error("qbit import failed", "error", msg)
		job.update(func(sn *importSnapshot) { sn.Phase = "error"; sn.Error = msg })
	}
	slog.Info("qbit import: starting", "url", req.URL)

	cl, err := newQbitClient(req.URL)
	if err != nil {
		fail(err.Error())
		return
	}
	if err := cl.login(req.Username, req.Password); err != nil {
		fail(err.Error())
		return
	}
	ts, err := cl.torrentsInfo()
	if err != nil {
		fail(err.Error())
		return
	}
	cats, _ := cl.categories()
	slog.Info("qbit import: torrent list fetched", "total", len(ts))

	if s.hoardEngine == nil {
		fail("hoard engine not available")
		return
	}

	// 1. Pre-create every needed category as HOARD mode BEFORE any add, so no
	//    torrent transiently falls into the race default.
	job.update(func(sn *importSnapshot) { sn.Phase = "categories"; sn.Total = len(ts) })
	existing := loadCategories(s.config.Daemon.DataDir)
	byName := map[string]bool{}
	for _, ec := range existing {
		byName[strings.ToLower(ec.Name)] = true
	}
	needed := map[string]string{}
	for _, t := range ts {
		cn := t.Category
		if cn == "" {
			cn = "imported"
		}
		sp := firstNonEmpty(qbitCatSavePath(cats, t.Category), t.SavePath)
		needed[cn] = remapPath(sp, req.PathMap)
	}
	changed := false
	for name, sp := range needed {
		if byName[strings.ToLower(name)] {
			continue // never clobber an existing category
		}
		existing = append(existing, category{Name: name, SavePath: sp, Mode: "hoard"})
		byName[strings.ToLower(name)] = true
		changed = true
	}
	if changed {
		if err := saveCategories(s.config.Daemon.DataDir, existing); err != nil {
			fail("save categories: " + err.Error())
			return
		}
	}

	// 2. Add torrents: completed -> seed-mode, partial -> normal add.
	job.update(func(sn *importSnapshot) { sn.Phase = "torrents" })
	tmpDir := filepath.Join(s.config.Daemon.DataDir, "uploads")
	os.MkdirAll(tmpDir, 0755)
	// Pipelined add: a small pool of fetchers pulls .torrent files from qBit
	// (its WebUI is roughly single-threaded, so keep this low to avoid stalling
	// its UI) feeding a larger pool of adders hitting the concurrency-safe engine
	// RPC. Shared counters are mutex-guarded; job.update is already thread-safe.
	const fetchWorkers = 6
	const addWorkers = 10

	type addItem struct {
		t    qbitTorrent
		tmp  string
		seed bool
	}

	var mu sync.Mutex
	var carried, carriedDL, earliest int64
	// Tally failure reasons (bucketed) so the completion log explains WHY
	// torrents failed without one line per torrent. Guarded by mu.
	failTally := map[string]int{}
	tallyFail := func(stage, e string) {
		k := classifyImportErr(stage, e)
		mu.Lock()
		failTally[k]++
		mu.Unlock()
	}

	addCh := make(chan addItem, addWorkers*2)
	var addWG sync.WaitGroup
	for i := 0; i < addWorkers; i++ {
		addWG.Add(1)
		go func() {
			defer addWG.Done()
			for it := range addCh {
				name := it.t.Name
				cn := it.t.Category
				if cn == "" {
					cn = "imported"
				}
				sp := remapPath(it.t.SavePath, req.PathMap)
				var ih string
				var addErr error
				if it.seed {
					// Replicate qBit exact on-disk layout from content_path: stat tells multi
					// (a dir) from single (the file); derive the content dir + whether the
					// torrent has its own subfolder so shim/move behave correctly.
					resolvedSave := sp
					var cf *bool
					if cp := remapPath(it.t.ContentPath, req.PathMap); cp != "" {
						if fi, statErr := os.Stat(cp); statErr == nil {
							v := true
							if fi.IsDir() {
								resolvedSave = cp
							} else {
								resolvedSave = filepath.Dir(cp)
								v = filepath.Dir(cp) != sp
							}
							cf = &v
						}
					}
					ih, addErr = s.hoardEngine.AddTorrentSeedMode(it.tmp, resolvedSave, cn)
					if addErr == nil && cf != nil {
						s.hoardEngine.SetContentFolder(ih, cf)
					}
				} else {
					ih, addErr = s.hoardEngine.AddTorrent(it.tmp, sp, cn)
				}
				// Keep the .torrent in uploads/: the durable store captures the
				// metainfo BLOB from TorrentFilePath at sync time.
				if addErr != nil {
					el := strings.ToLower(addErr.Error())
					if strings.Contains(el, "already") || strings.Contains(el, "duplicate") || strings.Contains(el, "exists") {
						job.update(func(sn *importSnapshot) { sn.Skipped++; sn.Done++ })
						continue
					}
					slog.Warn("qbit import: add failed", "name", name, "seed_mode", it.seed, "save_path", sp, "error", addErr)
					tallyFail("add", addErr.Error())
					job.update(func(sn *importSnapshot) { sn.Failed++; sn.Done++; appendErr(sn, name+": add: "+addErr.Error()) })
					continue
				}
				// Preserve the original qBit add date instead of "now".
				if ih != "" && it.t.AddedOn > 0 {
					s.hoardEngine.SetAddedTime(ih, time.Unix(it.t.AddedOn, 0))
				}
				// Carry over a deliberate stop. Done right after the add so the
				// bootstrap announce (fired in its own goroutine) finds the
				// intent already set and stays quiet -- otherwise every stopped
				// torrent would greet the tracker and immediately say goodbye.
				if ih != "" && importedAsStopped(it.t.State) {
					if err := s.hoardEngine.SetUserPaused(ih, true); err != nil {
						slog.Warn("qbit import: could not carry the stopped state",
							"name", name, "error", err)
					} else {
						persistPaused([]string{ih}, true)
					}
				}
				seed := it.seed
				mu.Lock()
				carried += it.t.Uploaded
				carriedDL += it.t.Downloaded
				if it.t.AddedOn > 0 && (earliest == 0 || it.t.AddedOn < earliest) {
					earliest = it.t.AddedOn
				}
				mu.Unlock()
				job.update(func(sn *importSnapshot) {
					sn.Done++
					sn.Current = name
					if seed {
						sn.Seeded++
					} else {
						sn.Downloading++
					}
				})
			}
		}()
	}

	hashCh := make(chan qbitTorrent, fetchWorkers*2)
	var fetchWG sync.WaitGroup
	for i := 0; i < fetchWorkers; i++ {
		fetchWG.Add(1)
		go func() {
			defer fetchWG.Done()
			for t := range hashCh {
				name := t.Name
				data, err := cl.exportTorrent(t.Hash)
				if err != nil {
					tallyFail("export", err.Error())
					job.update(func(sn *importSnapshot) { sn.Failed++; sn.Done++; appendErr(sn, name+": export: "+err.Error()) })
					continue
				}
				tmp := filepath.Join(tmpDir, "qbimport-"+t.Hash+".torrent")
				if err := os.WriteFile(tmp, data, 0644); err != nil {
					tallyFail("write", err.Error())
					job.update(func(sn *importSnapshot) { sn.Failed++; sn.Done++; appendErr(sn, name+": write: "+err.Error()) })
					continue
				}
				addCh <- addItem{t: t, tmp: tmp, seed: t.Progress >= 1.0}
			}
		}()
	}

	for _, t := range ts {
		hashCh <- t
	}
	close(hashCh)
	fetchWG.Wait()
	close(addCh)
	addWG.Wait()

	// 3. Record provenance (drives the overview "carried over from ..." line).
	prov, _ := loadProvenance(s.config.Daemon.DataDir)
	prov.SourceClient = "qBittorrent"
	if earliest > 0 && (prov.SourceDate == 0 || earliest < prov.SourceDate) {
		prov.SourceDate = earliest
	}
	if prov.SourceDate == 0 {
		prov.SourceDate = time.Now().Unix()
	}
	prov.CarriedUploadedBytes += carried
	final := job.get()
	prov.ImportedCount += final.Seeded + final.Downloading
	_ = saveProvenance(s.config.Daemon.DataDir, prov)

	// Fold the historical UL/DL into the persistent all-time baseline so the
	// overview ratio/totals reflect the imported history (not just the cosmetic
	// "carried over" line). Skipped/failed torrents never reached these counters,
	// so a re-import does not double-count.
	atomic.AddInt64(&baselineUploaded, carried)
	atomic.AddInt64(&baselineDownloaded, carriedDL)
	saveBaseline()

	if s.saveStateFn != nil {
		s.saveStateFn()
	}
	if len(failTally) > 0 {
		type fr struct {
			reason string
			n      int
		}
		frs := make([]fr, 0, len(failTally))
		for r, n := range failTally {
			frs = append(frs, fr{r, n})
		}
		sort.Slice(frs, func(i, j int) bool { return frs[i].n > frs[j].n })
		parts := make([]string, 0, len(frs))
		for i, f := range frs {
			if i >= 15 {
				parts = append(parts, "...")
				break
			}
			parts = append(parts, fmt.Sprintf("%s x%d", f.reason, f.n))
		}
		slog.Warn("qbit import: failure breakdown", "reasons", strings.Join(parts, " | "))
	}
	slog.Info("qbit import: complete", "seeded", final.Seeded, "downloading", final.Downloading, "skipped", final.Skipped, "failed", final.Failed)
	job.update(func(sn *importSnapshot) { sn.Phase = "done" })
}

// handleQbitEvents streams the running import's progress over SSE.
func (s *Server) handleQbitEvents(c *gin.Context) {
	importMu.Lock()
	job := importCurrent
	importMu.Unlock()
	if job == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "no import job"})
		return
	}
	c.Writer.Header().Set("Content-Type", "text/event-stream")
	c.Writer.Header().Set("Cache-Control", "no-cache")
	c.Writer.Header().Set("Connection", "keep-alive")
	c.Writer.Header().Set("X-Accel-Buffering", "no")
	c.Writer.WriteHeader(http.StatusOK)
	flusher, ok := c.Writer.(http.Flusher)
	if !ok {
		c.AbortWithStatus(http.StatusInternalServerError)
		return
	}
	id, ch, initial := job.subscribe()
	defer job.unsubscribe(id)

	writeSnap := func(sn importSnapshot) {
		b, _ := json.Marshal(sn)
		fmt.Fprintf(c.Writer, "data: %s\n\n", b)
		flusher.Flush()
	}
	writeSnap(initial)
	if initial.Finished {
		return
	}
	ctx := c.Request.Context()
	for {
		select {
		case sn := <-ch:
			writeSnap(sn)
			if sn.Finished {
				return
			}
		case <-ctx.Done():
			return
		case <-time.After(30 * time.Second):
			fmt.Fprintf(c.Writer, ": ping\n\n")
			flusher.Flush()
		}
	}
}

// handleQbitStatus returns the current/last import snapshot (for UI resume).
func (s *Server) handleQbitStatus(c *gin.Context) {
	importMu.Lock()
	job := importCurrent
	importMu.Unlock()
	if job == nil {
		c.JSON(http.StatusOK, gin.H{"running": false})
		return
	}
	c.JSON(http.StatusOK, job.get())
}

// handleProvenance exposes the recorded migration origin so the overview can
// render a real "carried over from <client> since <date>" line (or hide it).
func (s *Server) handleProvenance(c *gin.Context) {
	p, ok := loadProvenance(s.config.Daemon.DataDir)
	if !ok {
		c.JSON(http.StatusOK, gin.H{"present": false})
		return
	}
	c.JSON(http.StatusOK, gin.H{
		"present":                true,
		"source_client":          p.SourceClient,
		"source_date":            p.SourceDate,
		"carried_uploaded_bytes": p.CarriedUploadedBytes,
		"imported_count":         p.ImportedCount,
	})
}

// classifyImportErr buckets a raw import error into a stable category so the
// completion log can summarise why torrents failed (e.g. 300x "export: timeout"
// vs "export: http 404") without emitting one log line per torrent.
func classifyImportErr(stage, e string) string {
	el := strings.ToLower(e)
	switch {
	case strings.Contains(el, "timeout") || strings.Contains(el, "deadline exceeded"):
		return stage + ": timeout"
	case strings.Contains(el, "connection refused"):
		return stage + ": connection refused"
	case strings.Contains(el, "connection reset"):
		return stage + ": connection reset"
	case strings.Contains(el, "no such host") || strings.Contains(el, "lookup "):
		return stage + ": dns/host"
	case strings.Contains(el, "export http"):
		if i := strings.Index(el, "export http"); i >= 0 {
			return stage + ": " + strings.TrimSpace(el[i:])
		}
	}
	// Fallback: first 60 chars so distinct engine errors still group together.
	if len(el) > 60 {
		el = el[:60]
	}
	return stage + ": " + el
}
