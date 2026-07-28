package engine

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// WatchdogAlerter matches notify.Ntfy.Send(title, message, priority, tags).
type WatchdogAlerter func(title, message, priority, tags string)

// Pid returns the child engine process PID, or 0 if not started.
func (ep *EngineProcess) Pid() int {
	if ep.cmd != nil && ep.cmd.Process != nil {
		return ep.cmd.Process.Pid
	}
	return 0
}

type watchdogTarget struct {
	name     string // "race" | "hoard"
	proc     *EngineProcess
	rssLimit int64 // bytes; 0 disables the RSS check for this engine
}

// StartEngineWatchdog polls the child engine processes every 30s and, on death
// (zombie/gone) or runaway RSS, alerts via ntfy and triggers a graceful
// self-restart (SIGTERM to self -> existing shutdown flushes the surviving
// engine -> Docker's --restart policy brings up a clean process pair).
//
// Rationale: on 2026-07-09 the race engine ballooned to ~85 GB RSS (a leak
// under disk backpressure) and was OOM-killed. There was no in-process
// supervisor, so race stayed dead ~3h with the stats frozen and NO alert. This
// watchdog closes both gaps: liveness (respawn via container restart) and a
// preemptive memory ceiling that fires long before a box-wide OOM can take out
// the *other* engine (or hydra-go itself).
//
// RSS limits are per-engine because normal footprints differ by an order of
// magnitude (race ~2 GB for ~100 torrents vs hoard ~24 GB for ~24k torrents on
// a 125 GB box).
func StartEngineWatchdog(ctx context.Context, alert WatchdogAlerter, dumpDir string, race, hoard *EngineProcess, raceRSSLimit, hoardRSSLimit int64) {
	watchdogDumpDir = dumpDir
	targets := []watchdogTarget{
		{name: "race", proc: race, rssLimit: raceRSSLimit},
		{name: "hoard", proc: hoard, rssLimit: hoardRSSLimit},
	}
	go func() {
		t := time.NewTicker(30 * time.Second)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				for _, tg := range targets {
					if tg.proc == nil {
						continue
					}
					pid := tg.proc.Pid()
					if pid <= 0 {
						continue
					}
					alive, rss := procStat(pid)
					if !alive {
						alert(
							fmt.Sprintf("Hydra: moteur %s MORT", tg.name),
							fmt.Sprintf("Le process engine %s (pid %d) est mort/zombie. Restart auto de hydra-go.", tg.name, pid),
							"urgent", "skull,rotating_light",
						)
						slog.Error("watchdog: engine dead, triggering graceful self-restart", "engine", tg.name, "pid", pid)
						selfRestart()
						return
					}
					if tg.rssLimit > 0 && rss > tg.rssLimit {
						// Capture the memory composition (anon heap vs file/mmap)
						// BEFORE killing — the crash logs of 2026-07-09 were lost to
						// log rotation, so we snapshot to durable state instead. This
						// is the forensic hook for the still-unattributed heap leak;
						// jemalloc profile attribution lands here next.
						dumpPath := dumpEngineMem(tg.name, pid, rss)
						// Ask the engine's jemalloc to dump a heap profile (SIGUSR1
						// handler in main.rs -> prof.dump to $prof_prefix) BEFORE we
						// kill it — allocation-site attribution for the heap leak.
						_ = syscall.Kill(pid, syscall.SIGUSR1)
						time.Sleep(3 * time.Second)
						alert(
							fmt.Sprintf("Hydra: moteur %s memoire anormale", tg.name),
							fmt.Sprintf("RSS engine %s = %.1f GB (> seuil %.1f GB). Restart preventif avant OOM. Dump: %s",
								tg.name, float64(rss)/1e9, float64(tg.rssLimit)/1e9, dumpPath),
							"urgent", "warning,fire",
						)
						slog.Error("watchdog: engine RSS over limit, preemptive restart",
							"engine", tg.name, "pid", pid, "rss_bytes", rss, "limit_bytes", tg.rssLimit, "dump", dumpPath)
						selfRestart()
						return
					}
				}
			}
		}
	}()
	slog.Info("engine watchdog started",
		"race_rss_limit_gb", float64(raceRSSLimit)/1e9,
		"hoard_rss_limit_gb", float64(hoardRSSLimit)/1e9)
}

// procStat reads /proc/<pid>/stat and returns (alive, rssBytes). A process in
// state Z (zombie) or X (dead) counts as NOT alive. Reads the RSS field
// (resident set size in pages, overall field 24) and converts to bytes.
func procStat(pid int) (bool, int64) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return false, 0 // process gone
	}
	s := string(data)
	// The comm field (field 2) is parenthesised and can itself contain spaces
	// and parens, so anchor on the LAST ')': state is the token right after it.
	rp := strings.LastIndexByte(s, ')')
	if rp < 0 || rp+2 >= len(s) {
		return false, 0
	}
	// rest[0] == field 3 (state); overall field N maps to rest[N-3].
	rest := strings.Fields(s[rp+2:])
	if len(rest) < 1 {
		return false, 0
	}
	if state := rest[0]; state == "Z" || state == "X" || state == "x" {
		return false, 0
	}
	// rss is overall field 24 -> rest[21].
	if len(rest) > 21 {
		if pages, err := strconv.ParseInt(rest[21], 10, 64); err == nil {
			return true, pages * int64(os.Getpagesize())
		}
	}
	return true, 0
}

// watchdogDumpDir follows HYDRA_CONFIG_DIR (the config/data volume) so crash
// dumps land on the persistent mount; defaults to /config.
var watchdogDumpDir = func() string {
	if d := os.Getenv("HYDRA_CONFIG_DIR"); d != "" {
		return d
	}
	return "/config"
}()

// dumpEngineMem snapshots the engine's memory composition to durable state
// before the watchdog kills it. smaps_rollup gives anon (heap) vs file-backed
// (mmap) totals; status gives VmRSS/VmData/VmSwap; the fd count rules out a
// descriptor leak. Best-effort — returns the path written (or "" on failure).
func dumpEngineMem(name string, pid int, rss int64) string {
	path := watchdogDumpDir + "/heapdump-" + name + "-pid" + strconv.Itoa(pid) + ".txt"
	var b strings.Builder
	fmt.Fprintf(&b, "watchdog memory dump: engine=%s pid=%d rss_bytes=%d\n\n", name, pid, rss)
	for _, f := range []string{"smaps_rollup", "status"} {
		src := "/proc/" + strconv.Itoa(pid) + "/" + f
		fmt.Fprintf(&b, "===== %s =====\n", src)
		if d, err := os.ReadFile(src); err == nil {
			b.Write(d)
		} else {
			fmt.Fprintf(&b, "(read error: %v)\n", err)
		}
		b.WriteByte('\n')
	}
	if entries, err := os.ReadDir("/proc/" + strconv.Itoa(pid) + "/fd"); err == nil {
		fmt.Fprintf(&b, "===== open fd count: %d =====\n", len(entries))
	}
	if err := os.WriteFile(path, []byte(b.String()), 0644); err != nil {
		slog.Error("watchdog: mem dump write failed", "path", path, "err", err)
		return ""
	}
	return path
}

// selfRestart triggers the existing SIGTERM graceful shutdown (saveState +
// engine flush). Docker's `--restart unless-stopped` then reboots a clean pair.
func selfRestart() {
	_ = syscall.Kill(os.Getpid(), syscall.SIGTERM)
}
