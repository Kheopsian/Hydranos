// Command agentprobe exercises the HydraAgent gRPC contract (the EngineClient
// mirror) against a running agent, for manual E2E validation. Read-only by
// default: Ping/ListTorrents/GetSessionStats/GetDiagnostics on each engine,
// plus a Subscribe probe (expected to be gated off in additive mode).
package main

import (
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"strings"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/btmeta"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

func main() {
	addr := flag.String("addr", "127.0.0.1:9099", "HydraAgent host:port")
	token := flag.String("token", "", "bearer token (defaults to $"+agentwire.TokenEnv+", the same variable the agent reads)")
	ca := flag.String("ca", "", "TLS CA cert path (empty = plaintext)")
	engines := flag.String("engines", "race,hoard", "comma-separated engines to probe")
	// Opt-in actions. The default probe stays read-only; each of these is a
	// deliberate single operation on ONE engine (-engines picks it), so a
	// mistyped command cannot mutate an agent that was only meant to be read.
	addFile := flag.String("add", "", "action: add this local .torrent to the agent (requires -save-path)")
	savePath := flag.String("save-path", "", "save_path for -add")
	seedMode := flag.Bool("seed-mode", false, "-add: adopt the payload as already complete (no re-download)")
	exportIH := flag.String("export", "", "action: print the durable state record of this info_hash as JSON")
	importFile := flag.String("import", "", "action: adopt the state record in this JSON file (writes)")
	adoptFrom := flag.String("adopt-from", "", "action: host:port of a SOURCE agent to adopt a torrent from (with -ih); ships identity+progression, never payload bytes")
	adoptIH := flag.String("ih", "", "info_hash to adopt with -adopt-from")
	srcToken := flag.String("src-token", "", "bearer token for -adopt-from (defaults to -token)")
	verifyIH := flag.String("verify", "", "action: recheck this info_hash against the bytes on disk (rebuilds the bitfield)")
	moveFrom := flag.String("move-from", "", "action: host:port of a SOURCE agent to MOVE a torrent from (with -ih and -save-path); transfers the payload piece by piece")
	flag.Parse()
	// Probing from the box that serves the agent is the common case, and that
	// box already exports the token: read it rather than making the operator
	// paste a secret into a shell that keeps its history.
	if *token == "" {
		*token = strings.TrimSpace(os.Getenv(agentwire.TokenEnv))
	}

	// An action targets exactly one engine: running it once per engine in the
	// default "race,hoard" list would add or adopt the same torrent twice.
	if *addFile != "" || *exportIH != "" || *importFile != "" || *adoptFrom != "" || *moveFrom != "" || *verifyIH != "" {
		eng := strings.TrimSpace(*engines)
		if strings.Contains(eng, ",") {
			fmt.Println("actions target a single engine: pass -engines <one>")
			os.Exit(2)
		}
		c, err := grpcclient.New(grpcclient.Config{Addr: *addr, Engine: eng, Token: *token, TLSCa: *ca})
		if err != nil {
			fmt.Printf("[%s] dial/ping FAILED: %v\n", eng, err)
			os.Exit(1)
		}
		defer c.Close()
		if *verifyIH != "" {
			if err := c.VerifyTorrent(*verifyIH); err != nil {
				fmt.Printf("[%s] verify FAILED: %v\n", eng, err)
				os.Exit(1)
			}
			fmt.Printf("[%s] recheck requested for %s\n", eng, *verifyIH)
			return
		}
		if *moveFrom != "" {
			st := *srcToken
			if st == "" {
				st = *token
			}
			if err := movePayload(c, eng, *moveFrom, st, *ca, *adoptIH, *savePath); err != nil {
				fmt.Printf("[%s] move FAILED: %v\n", eng, err)
				os.Exit(1)
			}
			return
		}
		if *adoptFrom != "" {
			st := *srcToken
			if st == "" {
				st = *token
			}
			if err := adopt(c, eng, *adoptFrom, st, *ca, *adoptIH, *savePath); err != nil {
				fmt.Printf("[%s] adopt FAILED: %v\n", eng, err)
				os.Exit(1)
			}
			return
		}
		if err := runAction(c, eng, *addFile, *savePath, *seedMode, *exportIH, *importFile); err != nil {
			fmt.Printf("[%s] FAILED: %v\n", eng, err)
			os.Exit(1)
		}
		return
	}

	// Node-level, once per agent rather than per engine: which configuration
	// the node is running and where it came from. A source other than "front"
	// on a node with a front is the first thing to check when an agent is not
	// behaving like its fleet.
	probeConfigState(*addr, *token, *ca)

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

// probeConfigState reports the node's configuration revision and the state of
// each declared engine. An agent that predates the pushed-config model answers
// Unimplemented, which is a fact about the node worth printing rather than a
// probe failure.
func probeConfigState(addr, token, ca string) {
	c, err := grpcclient.New(grpcclient.Config{Addr: addr, Engine: agentwire.EngineRace, Token: token, TLSCa: ca})
	if err != nil {
		return // the per-engine loop below reports the dial failure properly
	}
	defer c.Close()
	st, err := c.GetConfigState()
	if err != nil {
		fmt.Printf("[node] config_state unavailable: %v\n", err)
		return
	}
	fmt.Printf("[node] config revision=%d source=%s engines=%d\n", st.Revision, st.Source, len(st.Engines))
	for id, es := range st.Engines {
		line := fmt.Sprintf("[node]   %s role=%s port=%d state=%s", id, es.Role, es.ListenPort, es.State)
		if es.Error != "" {
			line += " error=" + es.Error
		}
		fmt.Println(line)
	}
}

// runAction performs one opt-in mutation/read against a single engine.
func runAction(c *grpcclient.Client, eng, addFile, savePath string, seedMode bool, exportIH, importFile string) error {
	switch {
	case addFile != "":
		if savePath == "" {
			return fmt.Errorf("-add requires -save-path")
		}
		res, err := c.AddTorrentWithOptions(addFile, savePath, false, seedMode)
		if err != nil {
			return err
		}
		fmt.Printf("[%s] added info_hash=%s name=%q save_path=%q seed_mode=%v\n",
			eng, res.InfoHash, res.Name, savePath, seedMode)

	case exportIH != "":
		rec, err := c.ExportState(exportIH)
		if err != nil {
			return err
		}
		b, mErr := json.MarshalIndent(rec, "", "  ")
		if mErr != nil {
			return mErr
		}
		fmt.Println(string(b))

	case importFile != "":
		raw, err := os.ReadFile(importFile)
		if err != nil {
			return err
		}
		var rec ltclient.ResumeRecord
		if err := json.Unmarshal(raw, &rec); err != nil {
			return err
		}
		ih, err := c.ImportState(&rec)
		if err != nil {
			return err
		}
		fmt.Printf("[%s] imported info_hash=%s save_path=%q\n", eng, ih, rec.SavePath)
	}
	return nil
}

// adopt carries one torrent's identity and progression from a source agent to
// dst, .torrent bytes included so the record is adoptable on a host that has
// never seen the file.
//
// It moves NO payload and it does NOT remove the torrent from the source: both
// ends hold it when this returns. That is deliberate for a probe -- the source
// stays the working copy if anything about the destination is wrong.
func adopt(dst *grpcclient.Client, eng, srcAddr, srcToken, ca, ih, savePath string) error {
	if ih == "" {
		return fmt.Errorf("-adopt-from requires -ih")
	}
	src, err := grpcclient.New(grpcclient.Config{Addr: srcAddr, Engine: eng, Token: srcToken, TLSCa: ca})
	if err != nil {
		return fmt.Errorf("dial source %s: %w", srcAddr, err)
	}
	defer src.Close()

	rec, err := src.ExportState(ih)
	if err != nil {
		return fmt.Errorf("export from %s: %w", srcAddr, err)
	}
	blob, err := src.GetTorrentFile(ih)
	if err != nil {
		return fmt.Errorf("get .torrent from %s: %w", srcAddr, err)
	}
	fmt.Printf("[%s] exported %s: save_path=%q seed_mode=%v ul=%d dl=%d bitfield=%dB trackers=%d torrent=%dB\n",
		eng, rec.InfoHash, rec.SavePath, rec.SeedMode, rec.TotalUploaded, rec.TotalDownloaded,
		len(rec.Bitfield)/2, len(rec.Trackers), len(blob))

	// The destination almost never stores the payload at the same path as the
	// source -- a different OS, a different mount, a cross-seed that already
	// had the data somewhere else. save_path is taken at face value on adopt,
	// so if it is not rewritten here the torrent lands pointing at a directory
	// that does not exist on this host.
	if savePath != "" {
		fmt.Printf("[%s] save_path rewritten %q -> %q\n", eng, rec.SavePath, savePath)
		rec.SavePath = savePath
	}

	got, err := dst.ImportStateWithFile(rec, blob)
	if err != nil {
		return fmt.Errorf("import: %w", err)
	}
	if got != rec.InfoHash {
		return fmt.Errorf("adopted info_hash %s != exported %s", got, rec.InfoHash)
	}
	fmt.Printf("[%s] adopted %s (payload NOT moved; source still holds it)\n", eng, got)
	return nil
}

// movePayload carries a torrent AND its bytes from a source agent to dst.
//
// Ordering, and why:
//  1. adopt on the destination PAUSED and with an EMPTY bitfield. Copying the
//     source's bitfield here would be a lie -- the destination has not got a
//     single byte yet -- and a torrent that believes it is complete will
//     happily serve zeros to a swarm. Paused keeps it from announcing while
//     it is still a shell.
//  2. transfer every piece. Each one is hash-checked by the receiver before it
//     touches the disk, so the transport is never trusted.
//  3. recheck, which turns the bytes on disk into the destination's real
//     bitfield.
//  4. only then is it the destination's to start -- and only then may the
//     source be released, which this probe deliberately does NOT do.
//
// The source keeps its data and keeps seeding throughout: if any step fails,
// the working copy is still the one that was working.
func movePayload(dst *grpcclient.Client, eng, srcAddr, srcToken, ca, ih, savePath string) error {
	if ih == "" || savePath == "" {
		return fmt.Errorf("-move-from requires -ih and -save-path")
	}
	src, err := grpcclient.New(grpcclient.Config{Addr: srcAddr, Engine: eng, Token: srcToken, TLSCa: ca})
	if err != nil {
		return fmt.Errorf("dial source %s: %w", srcAddr, err)
	}
	defer src.Close()

	rec, err := src.ExportState(ih)
	if err != nil {
		return fmt.Errorf("export: %w", err)
	}
	blob, err := src.GetTorrentFile(ih)
	if err != nil {
		return fmt.Errorf("get .torrent: %w", err)
	}
	layout, err := btmeta.ParseLayout(blob)
	if err != nil {
		return fmt.Errorf("parse layout: %w", err)
	}
	fmt.Printf("[%s] source %s: %d pieces of %d bytes, %d bytes total, multi_file=%v\n",
		eng, ih, layout.NumPieces(), layout.PieceLength, layout.TotalSize, layout.MultiFile)

	// Adopt only if this is a fresh move. A destination that already holds the
	// torrent is an interrupted transfer being picked up, and re-adopting would
	// throw away the very bitfield that says how far it got.
	have, err := dst.ExportState(ih)
	switch {
	case err != nil:
		rec.SavePath = savePath
		rec.Bitfield = ""
		rec.Paused = true
		rec.SeedMode = false
		if _, aErr := dst.ImportStateWithFile(rec, blob); aErr != nil {
			return fmt.Errorf("adopt on destination: %w", aErr)
		}
		fmt.Printf("[%s] adopted on destination, paused, empty bitfield, save_path=%q\n", eng, savePath)
	default:
		fmt.Printf("[%s] destination already holds it, resuming (save_path=%q)\n", eng, have.SavePath)
	}

	// The resume point is the destination's own bitfield. Nothing else is
	// consulted and nothing else needs to be: a piece is present exactly when
	// the destination's engine hashed it and said so.
	present, err := presentPieces(dst, ih, layout.NumPieces())
	if err != nil {
		return err
	}
	todo := layout.NumPieces() - len(present)
	fmt.Printf("[%s] destination has %d/%d pieces, %d to transfer\n",
		eng, len(present), layout.NumPieces(), todo)

	var moved int64
	var sent int
	for i := 0; i < layout.NumPieces(); i++ {
		if present[i] {
			continue
		}
		data, err := src.ReadPiece(ih, i)
		if err != nil {
			return fmt.Errorf("read piece %d: %w", i, err)
		}
		if err := dst.WritePiece(ih, i, data); err != nil {
			return fmt.Errorf("write piece %d: %w", i, err)
		}
		moved += int64(len(data))
		sent++
	}
	fmt.Printf("[%s] transferred %d pieces, %d bytes (skipped %d already present)\n",
		eng, sent, moved, len(present))

	if err := dst.VerifyTorrent(ih); err != nil {
		return fmt.Errorf("recheck on destination: %w", err)
	}
	fmt.Printf("[%s] recheck requested on destination (source untouched, still holds the payload)\n", eng)
	return nil
}

// presentPieces reads the destination's bitfield and returns the set of pieces
// it already holds.
func presentPieces(c *grpcclient.Client, ih string, n int) (map[int]bool, error) {
	rec, err := c.ExportState(ih)
	if err != nil {
		return nil, fmt.Errorf("reading destination bitfield: %w", err)
	}
	out := map[int]bool{}
	if rec.Bitfield == "" {
		// No picker means either nothing yet, or a seed_mode torrent that
		// considers itself complete. During a move it is always the former:
		// the adopt above deliberately clears seed_mode.
		return out, nil
	}
	raw, err := hex.DecodeString(rec.Bitfield)
	if err != nil {
		return nil, fmt.Errorf("destination bitfield is not hex: %w", err)
	}
	for i := 0; i < n; i++ {
		if i/8 < len(raw) && raw[i/8]>>(7-uint(i%8))&1 == 1 {
			out[i] = true
		}
	}
	return out, nil
}
