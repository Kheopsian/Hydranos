// Command agentprobe exercises the HydraAgent gRPC contract (the EngineClient
// mirror) against a running agent, for manual E2E validation. Read-only by
// default: Ping/ListTorrents/GetSessionStats/GetDiagnostics on each engine,
// plus a Subscribe probe (expected to be gated off in additive mode).
package main

import (
	"flag"
	"fmt"
	"os"
	"strings"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
)

func main() {
	addr := flag.String("addr", "127.0.0.1:9099", "HydraAgent host:port")
	token := flag.String("token", "", "bearer token (defaults to $"+agentwire.TokenEnv+", the same variable the agent reads)")
	ca := flag.String("ca", "", "TLS CA cert path (empty = plaintext)")
	engines := flag.String("engines", "race,hoard", "comma-separated engines to probe")
	flag.Parse()
	// Probing from the box that serves the agent is the common case, and that
	// box already exports the token: read it rather than making the operator
	// paste a secret into a shell that keeps its history.
	if *token == "" {
		*token = strings.TrimSpace(os.Getenv(agentwire.TokenEnv))
	}

	fail := false
	for _, eng := range strings.Split(*engines, ",") {
		eng = strings.TrimSpace(eng)
		if eng == "" {
			continue
		}
		c, err := grpcclient.New(grpcclient.Config{Addr: *addr, Engine: eng, Token: *token, TLSCa: *ca})
		if err != nil {
			fmt.Printf("[%s] dial/ping FAILED: %v\n", eng, err)
			fail = true
			continue
		}
		list, err := c.ListTorrents()
		if err != nil {
			fmt.Printf("[%s] ListTorrents FAILED: %v\n", eng, err)
			fail = true
			c.Close()
			continue
		}
		stats, err := c.GetSessionStats()
		if err != nil {
			fmt.Printf("[%s] GetSessionStats FAILED: %v\n", eng, err)
			fail = true
			c.Close()
			continue
		}
		diag, derr := c.GetDiagnostics()
		diagCounters := -1
		if derr == nil && diag != nil {
			diagCounters = len(diag.Counters)
		}
		// Subscribe: in additive mode the agent gates this off; a clean
		// Unavailable is the expected, correct result (not a failure).
		subErr := c.SubscribeEvents()
		subNote := "stream-open-OK"
		if subErr != nil {
			subNote = "gated(" + subErr.Error() + ")"
		}
		fmt.Printf("[%s] ping=OK torrents=%d count=%d stats{numT=%d ul=%d dl=%d ulRate=%d} diagCounters=%d subscribe=%s\n",
			eng, len(list.Torrents), list.Count, stats.NumTorrents, stats.TotalUpload, stats.TotalDownload, stats.UploadRate, diagCounters, subNote)
		c.Close()
	}
	if fail {
		os.Exit(1)
	}
}
