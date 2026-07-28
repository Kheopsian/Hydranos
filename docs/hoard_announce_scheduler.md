# Spec — Refonte de l'annonce hoard : scheduler central + pool de workers (Option B)

**Statut** : proposée 2026-07-22. **Objectif** : supprimer le coût CPU O(goroutines) de l'annonce hoard (aujourd'hui 1 goroutine persistante/torrent → ~7 cœurs de GC scanobject à 65k). Cible de scalabilité : **200k+ torrents**.

## 0. Contexte / diagnostic
- Actuel (`internal/engine/hoard_announce.go`) : `scanAndStart()` spawn `go torrentAnnounceLoop(ih,...)` par torrent, parkée dans `sleepOrStop`. À N torrents = **N goroutines vivantes**. Le GC scanne N stacks/cycle → `runtime.scanobject` ~25% du CPU (profil pprof 2.9.9, 2026-07-22).
- Cible : goroutines = **1 (scheduler) + M (workers)**, constant. L'état/torrent devient un **struct plat** dans un heap/map, pas une goroutine.
- Débit à dimensionner : `N / intervalle`. À 200k / 1800s ≈ **111 ann/s** ; concurrence en vol = `débit × latence_p99 × marge` ≈ 111 × 2s × 2 ≈ **~450** → pool **M ≈ 512**.

## 1. Structures

```go
// announceState : état par torrent, PAS de goroutine. Pointer-light (garder le
// GC scan de la map bon marché à 200k+ : éviter les gros champs/maps ici).
type announceState struct {
    infoHash      string
    totalSize     int64
    interval      time.Duration // dernier interval renvoyé par le tracker
    firstAnnounce bool          // event=started au 1er tour
    nextAt        time.Time     // échéance planifiée
    heapIdx       int           // index dans le heap (maintenu par heap.Interface)
    inFlight      bool          // annonce en cours (évite double-dispatch)
}

// schedHeap : min-heap sur nextAt. Possédé EXCLUSIVEMENT par la goroutine
// scheduler (aucun lock). Implémente container/heap.Interface.
type schedHeap []*announceState

// job : envoyé du scheduler vers un worker.
type job struct {
    st *announceState // lecture seule côté worker sauf via result
}

// result : renvoyé du worker vers le scheduler pour reprogrammer/retirer.
type result struct {
    infoHash string
    nextIn   time.Duration // >0 = reprogrammer dans nextIn ; 0 = retirer
    remove   bool          // torrent parti (removed/paused/race-owned/erreur)
    obs      *Observation  // pour OnObservation (peut être nil)
}
```

`HoardAnnouncer` gagne :
```go
type HoardAnnouncer struct {
    // ... champs existants (client, raceHas, offsetFn, OnObservation, ctx, cancel)
    states map[string]*announceState // toutes les entrées connues (owned by scheduler)
    heap   schedHeap                  // owned by scheduler
    workCh   chan job                 // scheduler -> workers (buffer ~2*M)
    resultCh chan result              // workers -> scheduler (buffer ~2*M)
    workers  int                      // M (config)
}
```

## 2. Boucle scheduler (1 goroutine, propriétaire du heap/map, sans lock)

```go
func (h *HoardAnnouncer) runScheduler() {
    reconcile := time.NewTicker(hoardCycleInterval) // ex 30s : sync du set de torrents
    defer reconcile.Stop()
    timer := time.NewTimer(time.Hour)
    defer timer.Stop()

    h.reconcileTorrents() // 1er remplissage (respecte le stagger, cf §4)

    for {
        h.armTimer(timer) // reset sur (heap[0].nextAt - now), borné [0, 1h]
        select {
        case <-h.ctx.Done():
            return
        case <-timer.C:
            now := time.Now()
            // dispatch tous les échus, sans bloquer : si workCh plein -> stop,
            // on réessaiera au prochain tour (back-pressure naturelle).
            for len(h.heap) > 0 && !h.heap[0].nextAt.After(now) {
                st := h.heap[0]
                if st.inFlight { break } // ne devrait pas arriver
                select {
                case h.workCh <- job{st: st}:
                    st.inFlight = true
                    heap.Pop(&h.heap) // sortir du heap le temps de l'annonce
                default:
                    goto drained // workCh plein : back-pressure, on ré-arme le timer
                }
            }
        drained:
        case r := <-h.resultCh:
            st := h.states[r.infoHash]
            if st == nil { continue }
            st.inFlight = false
            if r.obs != nil && h.OnObservation != nil {
                h.OnObservation(r.infoHash, *r.obs)
            }
            if r.remove {
                delete(h.states, r.infoHash) // heapIdx déjà hors-heap
                continue
            }
            st.nextAt = time.Now().Add(r.nextIn)
            heap.Push(&h.heap, st) // ré-insertion
        case <-reconcile.C:
            h.reconcileTorrents()
        }
    }
}
```

## 3. Contrat worker (M goroutines identiques)

```go
func (h *HoardAnnouncer) runWorker() {
    for {
        select {
        case <-h.ctx.Done():
            return
        case j := <-h.workCh:
            st := j.st
            // TIMEOUT PAR ANNONCE — CRITIQUE : un tracker mort (ex secondary :1081)
            // ne doit PAS figer un worker.
            ctx, cancel := context.WithTimeout(h.ctx, announceHardTimeout) // ~30s
            nextIn, obs, gone := h.announceOnce(ctx, st) // réutilise announceAllTiers
            cancel()
            h.resultCh <- result{
                infoHash: st.infoHash,
                nextIn:   nextIn,
                remove:   gone,
                obs:      obs,
            }
        }
    }
}
```

`announceOnce(ctx, st)` = extraction de l'ancien corps de `torrentAnnounceLoop` (une itération) :
1. `GetStatus` (ou lecture du snapshot batch, cf §7) → si erreur/`!shouldAnnounce` → `gone=true`.
2. Gating race : `if h.raceHas(ih) { gone=true }` (cède l'annonce ; la reconciliation la reprendra quand le race purge).
3. Calcul `left` (garder la règle `left=1` pour leecher left<=0).
4. `event=started` si `st.firstAnnounce`, puis `st.firstAnnounce=false` si OK.
5. `GetTrackers` + `announceAllTiers(...)` → `newInterval, obs`.
6. Stamp `NextAt` par tracker dans `obs.Trackers` (comportement UI conservé).
7. `nextIn = clamp(newInterval, hoardMinInterval, ...)` ; retour.
- **Secondary announce** : reste fire-and-forget avec SON propre timeout court ; ne bloque jamais le worker.

## 4. Reconciliation (remplace `scanAndStart`)
`reconcileTorrents()` (dans le scheduler, sans lock) :
1. `list := h.client.ListTorrents()` (ou `GetAllStatus`, cf §7).
2. **Ajouts** : pour chaque torrent `shouldAnnounce` && !`raceHas` && absent de `states` → créer `announceState{firstAnnounce:true, nextAt: now + jitter}`, `heap.Push`. **Conserver le stagger `hoardMaxNewPerCycle`** : cap le nb d'ajouts/reconcile pour éviter le thundering-herd au boot (~200k → étaler sur plusieurs cycles). Ajouter un **jitter** (`rand` seedless via index) sur `nextAt` des nouveaux pour lisser la 1re vague.
3. **Retraits** : torrent dans `states` mais absent de la liste (ou plus `shouldAnnounce`) → si pas `inFlight`, retirer du heap + `delete(states)`. Si `inFlight`, laisser le result le nettoyer (`remove` déjà géré côté worker via `shouldAnnounce`).
4. `BootstrapAnnounce` : inchangé pour les adds frais (annonce immédiate hors scheduler) OU intégré en poussant `nextAt=now` sur le nouvel état.

## 5. Sizing / config (toml `[hoard_announce]`)
- `workers` (M) : défaut = `clamp(ceil(torrents_estimés / intervalle × p99 × 2), 64, 1024)`. Simplement exposer `workers=512` en config, ajustable. À 200k → 512.
- `work_buffer` = `2*M`, `result_buffer` = `2*M`.
- `announce_hard_timeout` = 30s.
- `max_new_per_cycle` (stagger) = garder l'actuel `hoardMaxNewPerCycle`.
- `reconcile_interval` = `hoardCycleInterval` (30s).

## 6. Feature flag + bascule mesurée
- Flag toml `hoard_announce_mode = "loop" | "scheduler"` (défaut `loop` d'abord).
- `main.go` : selon le flag, démarrer l'ancien `Run()` (goroutine/torrent) OU `runScheduler()` + M×`runWorker()`.
- **Bascule mesurée au pprof (dispo depuis 2.9.9, :6060 netns styx)** :
  - Avant : `goroutine profile: total` ≈ N (63k) ; `go tool pprof -top` → `scanobject` ~25%.
  - Après `scheduler` : `total` ≈ `M + qq` (~520) ; `scanobject` doit s'effondrer ; CPU hydra-go doit passer de ~7 cœurs à <1.
  - Vérifier aussi : annonces effectives (log `announceAllTiers` OK), pas de régression `last_error`, ratio jetons torr9 stable.
- Rollback = repasser le flag `loop` + restart.

## 7. Optimisations GC secondaires (bonus, après la bascille)
- **Batch status** : `GetAllStatus()` (1 RPC/cycle) indexé par infohash au lieu de `GetStatus` par torrent → coupe N RPC + allocs. Le worker lit le snapshot partagé (RWMutex, rafraîchi par le scheduler au reconcile).
- **Réutiliser les buffers d'observation** (`sync.Pool`) pour ne pas ré-allouer `obs.Trackers` à chaque annonce.
- Garder `announceState` **pointer-light** (pas de map/slice par entrée) → le scan GC de `states` reste O(N) cheap même à 1M.

## 8. Edge-cases / risques
- **Worker famine sur trackers lents** : le hard-timeout + M large l'évitent ; surveiller `workCh` len (métrique) → si souvent plein, bump M.
- **inFlight + retrait concurrent** : géré — un torrent `inFlight` est hors-heap ; son result le supprime si `gone`.
- **Shutdown** : `ctx.Cancel()` → scheduler et workers sortent ; drainer `workCh`/`resultCh` best-effort (les annonces en vol finissent ou timeout).
- **Ordre d'annonce** : le heap garantit l'ordre par échéance ; le stagger initial + jitter évite les vagues synchronisées.
- **Ne PAS réintroduire de goroutine/torrent** dans `announceOnce` (ex un `go` par tracker) — sinon on recrée le problème. Les tiers de trackers s'annoncent en série dans le worker (ou petit fan-out borné).

## 9. Découpage d'implémentation (PRs)
1. Extraire `announceOnce(ctx, st)` de `torrentAnnounceLoop` (refactor pur, sans changement de comportement, testé contre l'ancien).
2. Ajouter heap + scheduler + workers + flag `scheduler` (défaut OFF).
3. Bascule en prod derrière flag, mesure pprof avant/après.
4. Batch `GetAllStatus` + `sync.Pool` (optimisation GC secondaire).
5. Retirer l'ancien `torrentAnnounceLoop`/`scanAndStart` une fois `scheduler` validé N jours.
