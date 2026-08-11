package api

import (
	"net/http"
	"sync"
	"sync/atomic"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/sqlitex"
)

// Store repair mode.
//
// A database created on local disk is in WAL, and the fallback a share forces
// (nolock) cannot open a WAL file at all: SQLite fails the open outright,
// because WAL needs a -shm index that nolock disables. So pointing data_dir at
// a share leaves an install that will not start, for a reason no message used
// to explain and that nobody should be expected to fix with a SQLite shell.
//
// Rather than dying, or -- far worse -- carrying on without a store, Hydra
// comes up in this mode: the API answers, the front says what happened, and one
// authenticated button backs the databases up and converts them. The engines
// never start. That is the whole point. A daemon that ran on without its store
// rewrote its JSON sidecars from an empty memory and destroyed lifetime
// counters that cannot be rebuilt from anything.

// RepairTarget is one database and what is wrong with it.
type RepairTarget struct {
	Name string `json:"name"` // the file name, which is what the user sees on disk
	Path string `json:"path"`
	// HotWAL means the write-ahead log still holds changes the database does
	// not have. The safe conversion route is then the only one available, and
	// it needs a filesystem that can lock -- so this is the case we may have to
	// refuse, and the user has to hear why.
	HotWAL bool `json:"hot_wal"`
}

// RepairResult records what a repair run did to one database.
type RepairResult struct {
	Name   string `json:"name"`
	Backup string `json:"backup"`
	Method string `json:"method"` // "pragma", "header" or "already-rollback"
	Error  string `json:"error"`
}

// RepairState is the whole picture, as the browser sees it.
type RepairState struct {
	// Needed is true while at least one database is unopenable here.
	Needed bool `json:"needed"`
	// Filesystem names what was detected, for the message.
	Filesystem string         `json:"filesystem"`
	Targets    []RepairTarget `json:"targets"`

	// Filled in once the button has been pressed.
	Ran      bool           `json:"ran"`
	Repaired bool           `json:"repaired"`
	Results  []RepairResult `json:"results"`
}

var (
	repairState atomic.Pointer[RepairState]
	repairMu    sync.Mutex // one repair at a time: it rewrites files
	// repairMode is latched at boot and never cleared. Which routes exist was
	// decided then, so a converted database does not give this process its
	// engines back -- only a restart does. Clearing it would tell the browser
	// to load an interface whose endpoints were never registered, which looks
	// exactly like hanging forever on the boot screen.
	repairMode atomic.Bool
)

// SetRepairState puts the daemon into store-repair mode. Called at boot, before
// anything opens a database or starts an engine.
func SetRepairState(st *RepairState) {
	if st != nil && st.Needed {
		repairMode.Store(true)
	}
	repairState.Store(st)
}

// RepairNeeded reports whether this process came up in repair mode. It stays
// true until the process ends, however the repair went.
func RepairNeeded() bool { return repairMode.Load() }

func currentRepairState() *RepairState {
	if st := repairState.Load(); st != nil {
		return st
	}
	return &RepairState{}
}

// registerRepairRoutes serves the bare minimum needed to explain the problem
// and fix it: the front itself, the status the front asks for on load, a way to
// sign in, the repair, and a restart.
//
// Everything else is deliberately absent rather than returning errors. There is
// no engine behind those routes, and a half-working API invites the browser to
// act as though the daemon were running.
func (s *Server) registerRepairRoutes() {
	s.router.GET("/health", s.handleHealth)
	s.router.GET("/", s.handleIndex)
	s.router.GET("/api/setup", s.handleSetupStatus)
	// First-run account creation stays reachable here on purpose: an install
	// that has a database but never set a password would otherwise have no way
	// to authenticate, and so no way to press the one button on offer.
	s.router.POST("/api/setup", s.handleSetupPassword)
	s.router.POST("/api/login", s.handleLogin)
	s.router.GET("/api/store/repair", s.handleRepairStatus)

	auth := s.router.Group("/api", s.apiKeyAuth())
	auth.POST("/store/repair", s.handleRepairRun)
	auth.POST("/settings/restart", s.handleSettingsRestart)
}

// handleRepairStatus (public) describes the problem. It is readable without
// credentials for the same reason the storage warning is: it says nothing a
// failed start does not already say, and the browser has to be able to show the
// explanation before it can offer a login box.
func (s *Server) handleRepairStatus(c *gin.Context) {
	c.JSON(http.StatusOK, currentRepairState())
}

// handleRepairRun backs each database up, then converts it. Authenticated: it
// rewrites the user's data, which is not something an unauthenticated visitor
// gets to trigger.
//
// The backup comes first and a failure to make one stops that database being
// touched at all. Converting without a copy would be trading a recoverable
// problem for an unrecoverable one.
func (s *Server) handleRepairRun(c *gin.Context) {
	repairMu.Lock()
	defer repairMu.Unlock()

	st := currentRepairState()
	if !st.Needed {
		c.JSON(http.StatusOK, st)
		return
	}

	next := *st
	next.Ran = true
	next.Results = nil
	allOK := true

	for _, tgt := range st.Targets {
		res := RepairResult{Name: tgt.Name}

		backup, err := sqlitex.Backup(tgt.Path)
		if err != nil {
			res.Error = "could not back the database up, so nothing was changed: " + err.Error()
			allOK = false
			next.Results = append(next.Results, res)
			continue
		}
		res.Backup = backup

		method, err := sqlitex.Convert(tgt.Path)
		if err != nil {
			res.Error = err.Error()
			allOK = false
		} else {
			res.Method = method
		}
		next.Results = append(next.Results, res)
	}

	// Needed stays true on purpose. The databases open now, but this process
	// was built without a store and cannot grow one: its routes were registered
	// at boot. Only a restart finishes the job, so the browser must keep being
	// told it is talking to a daemon in repair mode.
	next.Repaired = allOK
	repairState.Store(&next)

	code := http.StatusOK
	if !allOK {
		code = http.StatusInternalServerError
	}
	c.JSON(code, next)
}
