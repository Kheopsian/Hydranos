package engine

// Scheduler central + pool de workers pour l'annonce hoard.
//
// Remplace le modèle "1 goroutine persistante par torrent" (orchestratorLoop +
// torrentAnnounceLoop) qui, à 65k+ torrents, laissait ~63k goroutines parkées
// dont le GC devait scanner tous les stacks -> runtime.scanobject ~25% CPU
// (~7 coeurs, diagnostiqué au pprof 2026-07-22). Ici les goroutines sont
// constantes : 1 scheduler + N workers, quel que soit le nombre de torrents.
//
// Concurrence : le scheduler est SEUL propriétaire de `states` et `heap`
// (aucun lock) ; les workers ne communiquent que par channels. announceAllTiers
// / OnObservation étaient déjà appelés en parallèle par les 63k goroutines,
// donc thread-safe.

import (
	"container/heap"
	"log/slog"
	"time"
)

const (
	hoardSchedWorkers = 512 // dimensionné pour ~200k torrents (débit x latence)
)

// announceState : état par torrent (struct plat, pas de goroutine).
type announceState struct {
	infoHash      string
	totalSize     int64
	firstAnnounce bool
	nextAt        time.Time
	heapIdx       int
	inFlight      bool
}

// schedHeap : min-heap sur nextAt (container/heap). Possédé par le scheduler.
type schedHeap []*announceState

func (h schedHeap) Len() int           { return len(h) }
func (h schedHeap) Less(i, j int) bool { return h[i].nextAt.Before(h[j].nextAt) }
func (h schedHeap) Swap(i, j int)      { h[i], h[j] = h[j], h[i]; h[i].heapIdx = i; h[j].heapIdx = j }
func (h *schedHeap) Push(x any) {
	st := x.(*announceState)
	st.heapIdx = len(*h)
	*h = append(*h, st)
}
func (h *schedHeap) Pop() any {
	old := *h
	n := len(old)
	st := old[n-1]
	old[n-1] = nil
	st.heapIdx = -1
	*h = old[:n-1]
	return st
}

type schedJob struct {
	infoHash  string
	totalSize int64
	first     bool
}

type schedResult struct {
	infoHash string
	nextIn   time.Duration
	remove   bool
}

type hoardScheduler struct {
	h        *HoardAnnouncer
	states   map[string]*announceState
	heap     schedHeap
	workCh   chan schedJob
	resultCh chan schedResult
}

// startScheduler lance le scheduler + le pool de workers. Appelé depuis Start()
// à la place de orchestratorLoop.
func (h *HoardAnnouncer) startScheduler() {
	s := &hoardScheduler{
		h:        h,
		states:   make(map[string]*announceState),
		heap:     make(schedHeap, 0, 4096),
		workCh:   make(chan schedJob, 2*hoardSchedWorkers),
		resultCh: make(chan schedResult, 2*hoardSchedWorkers),
	}
	for i := 0; i < hoardSchedWorkers; i++ {
		go s.worker()
	}
	s.run()
}

func (s *hoardScheduler) run() {
	// Boot delay : laisser Typhon finir de charger les torrents.
	select {
	case <-s.h.ctx.Done():
		return
	case <-time.After(hoardInitialBootDelay):
	}

	reconcile := time.NewTicker(hoardCycleInterval)
	defer reconcile.Stop()
	timer := time.NewTimer(time.Hour)
	defer timer.Stop()

	s.reconcile()

	for {
		// Armer le timer sur la prochaine échéance.
		d := time.Hour
		if len(s.heap) > 0 {
			d = time.Until(s.heap[0].nextAt)
			if d < 0 {
				d = 0
			}
		}
		if !timer.Stop() {
			select {
			case <-timer.C:
			default:
			}
		}
		timer.Reset(d)

		select {
		case <-s.h.ctx.Done():
			return

		case <-timer.C:
			now := time.Now()
		dispatch:
			for len(s.heap) > 0 && !s.heap[0].nextAt.After(now) {
				st := s.heap[0]
				select {
				case s.workCh <- schedJob{infoHash: st.infoHash, totalSize: st.totalSize, first: st.firstAnnounce}:
					st.inFlight = true
					heap.Pop(&s.heap)
				default:
					// workCh plein : back-pressure, on ré-arme et on réessaiera.
					break dispatch
				}
			}

		case r := <-s.resultCh:
			st := s.states[r.infoHash]
			if st == nil {
				continue
			}
			st.inFlight = false
			if r.remove {
				delete(s.states, r.infoHash)
				continue
			}
			st.firstAnnounce = false
			ni := r.nextIn
			if ni < hoardMinInterval {
				ni = hoardDefaultInterval
			}
			st.nextAt = time.Now().Add(ni)
			heap.Push(&s.heap, st)

		case <-reconcile.C:
			s.reconcile()
		}
	}
}

func (s *hoardScheduler) worker() {
	for {
		select {
		case <-s.h.ctx.Done():
			return
		case j := <-s.workCh:
			nextIn, gone := s.h.schedAnnounce(j.infoHash, j.totalSize, j.first)
			select {
			case s.resultCh <- schedResult{infoHash: j.infoHash, nextIn: nextIn, remove: gone}:
			case <-s.h.ctx.Done():
				return
			}
		}
	}
}

// reconcile synchronise l'ensemble planifié avec la liste live des torrents.
// Remplace scanAndStart : ajoute les nouveaux (staggerés), retire les partis.
func (s *hoardScheduler) reconcile() {
	// Pause de démarrage : ne rien planifier tant que le verrou tient. Le gate
	// dans announce() arrête déjà le trafic, mais sans ce court-circuit on
	// remplit quand même le heap et chaque torrent échu part en annonce pour
	// se faire refuser -- à 100k torrents, beaucoup de travail dont le seul
	// produit est un écran d'erreurs tracker décrivant un état voulu.
	if s.h.startupHeld() {
		return
	}
	res, err := s.h.client.ListTorrents()
	if err != nil {
		slog.Debug("hoard_announce sched: ListTorrents failed", "error", err)
		return
	}
	seen := make(map[string]struct{}, len(res.Torrents))
	added := 0
	for i := range res.Torrents {
		t := &res.Torrents[i]
		if !shouldAnnounce(t) {
			continue
		}
		if s.h.raceHas != nil && s.h.raceHas(t.InfoHash) {
			continue
		}
		seen[t.InfoHash] = struct{}{}
		if _, ok := s.states[t.InfoHash]; ok {
			continue
		}
		if added >= hoardMaxNewPerCycle { // stagger anti thundering-herd
			continue
		}
		added++
		st := &announceState{
			infoHash:      t.InfoHash,
			totalSize:     t.TotalSize,
			firstAnnounce: true,
			nextAt:        time.Now().Add(jitterDelay(t.InfoHash)),
		}
		s.states[t.InfoHash] = st
		heap.Push(&s.heap, st)
	}
	// Retraits : présents en mémoire mais plus dans la liste (ou plus
	// annonçables) et pas en vol -> sortir du heap.
	for ih, st := range s.states {
		if _, ok := seen[ih]; ok {
			continue
		}
		if st.inFlight {
			continue // le result le nettoiera
		}
		if st.heapIdx >= 0 && st.heapIdx < len(s.heap) {
			heap.Remove(&s.heap, st.heapIdx)
		}
		delete(s.states, ih)
	}
	if added > 0 {
		slog.Info("hoard_announce sched: reconciled",
			"new", added, "active", len(s.states), "total", len(res.Torrents))
	}
}

// schedAnnounce fait UNE annonce pour le scheduler et renvoie le prochain
// intervalle + un flag "torrent parti" (à retirer du planning). Miroir de
// announceOnce mais avec retour + event=started seulement au 1er tour.
func (h *HoardAnnouncer) schedAnnounce(infoHash string, totalSize int64, first bool) (time.Duration, bool) {
	// Race owns the announce -> cede (reconcile re-add si le race purge).
	if h.raceHas != nil && h.raceHas(infoHash) {
		return 0, true
	}
	status, err := h.client.GetStatus(infoHash)
	if err != nil {
		return 0, true // torrent parti / status KO -> drop (reconcile re-add si revient)
	}
	if !shouldAnnounce(status) {
		return 0, true
	}
	if totalSize <= 0 {
		totalSize = status.TotalSize
	}
	trackers, err := h.client.GetTrackers(infoHash)
	if err != nil || len(trackers) == 0 {
		return hoardMinInterval, false // mid-add : retenter bientôt
	}
	ulOff, dlOff := int64(0), int64(0)
	if h.offsetFn != nil {
		ulOff, dlOff = h.offsetFn(infoHash)
	}
	var left int64
	if status.IsSeeding {
		left = 0
	} else {
		left = totalSize - status.TotalDone
		if left <= 0 {
			left = 1 // leecher : jamais left=0 (le tracker retient les peers)
		}
	}
	event := ""
	if first {
		event = "started"
	}
	interval, obs := h.announceAllTiers(infoHash, totalSize, left,
		status.TotalUpload+ulOff, status.TotalDownload+dlOff, event, trackers)
	nextAt := time.Now().Add(interval)
	for url, to := range obs.Trackers {
		to.NextAt = nextAt
		obs.Trackers[url] = to
	}
	if h.OnObservation != nil {
		h.OnObservation(infoHash, obs)
	}
	if interval < hoardMinInterval {
		interval = hoardDefaultInterval
	}
	return interval, false
}

// jitterDelay étale la 1re annonce d'un nouveau torrent sur 0..30s (déterministe
// par infohash) pour lisser la vague d'ajout sans thundering-herd.
func jitterDelay(ih string) time.Duration {
	var s uint32
	for i := 0; i < len(ih); i++ {
		s = s*131 + uint32(ih[i])
	}
	return time.Duration(s%30000) * time.Millisecond
}
