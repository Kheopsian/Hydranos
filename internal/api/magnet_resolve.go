package api

import (
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/magnet"
)

// How long we let a resolution run before declaring it dead, and how often we
// ask. The engine caps its own job too; this is the caller-side ceiling.
const (
	magnetResolveTimeout = 5 * time.Minute
	magnetPollInterval   = 2 * time.Second
)

// metadataSource is whatever can resolve a magnet for us: a local engine, or a
// remote agent's client. Both satisfy it, which is how "the node that will hold
// the data resolves it" falls out of the existing routing.
type metadataSource interface {
	FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error)
	GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error)
}

// PendingMagnet is a magnet whose info dict has not arrived yet. It exists so
// the UI and the qBit shim can show something for a torrent that has no name,
// no size and no files -- qBittorrent calls this state metaDL.
type PendingMagnet struct {
	InfoHash string
	// Name is the magnet's `dn` hint, which may be absent or a lie. It is
	// replaced by the real name from the info dict once resolved.
	Name    string
	Mode    string
	Target  string
	State   string // resolving | failed
	Error   string
	Started time.Time
}

var pendingMagnets = struct {
	sync.RWMutex
	byHash map[string]*PendingMagnet
}{byHash: map[string]*PendingMagnet{}}

func magnetBegin(infoHash, name, mode, target string) {
	pendingMagnets.Lock()
	defer pendingMagnets.Unlock()
	pendingMagnets.byHash[infoHash] = &PendingMagnet{
		InfoHash: infoHash,
		Name:     name,
		Mode:     mode,
		Target:   target,
		State:    "resolving",
		Started:  time.Now(),
	}
}

func magnetFailed(infoHash string, err error) {
	pendingMagnets.Lock()
	defer pendingMagnets.Unlock()
	if p, ok := pendingMagnets.byHash[infoHash]; ok {
		p.State = "failed"
		p.Error = err.Error()
	}
}

// magnetResolved drops the placeholder: the real torrent now exists in the
// engine and takes over the listing.
func magnetResolved(infoHash string) {
	pendingMagnets.Lock()
	defer pendingMagnets.Unlock()
	delete(pendingMagnets.byHash, infoHash)
}

// PendingMagnets returns a snapshot of magnets still resolving (or failed and
// not yet acknowledged).
func PendingMagnets() []PendingMagnet {
	pendingMagnets.RLock()
	defer pendingMagnets.RUnlock()
	out := make([]PendingMagnet, 0, len(pendingMagnets.byHash))
	for _, p := range pendingMagnets.byHash {
		out = append(out, *p)
	}
	return out
}

// metadataSourceFor picks who resolves: the target that will hold the data.
func (s *Server) metadataSourceFor(target, mode string) (metadataSource, error) {
	if target != "local" {
		ra := s.remoteAgentByName(target)
		if ra == nil {
			return nil, fmt.Errorf("unknown agent %q", target)
		}
		return ra.anyClient(), nil
	}
	switch mode {
	case "race":
		if s.raceEngine == nil {
			return nil, fmt.Errorf("race engine not available")
		}
		return s.raceEngine, nil
	case "hoard":
		if s.hoardEngine == nil {
			return nil, fmt.Errorf("hoard engine not available")
		}
		return s.hoardEngine, nil
	}
	return nil, fmt.Errorf("invalid mode %q", mode)
}

// startMagnetResolve kicks off resolution and returns the info hash straight
// away. Blocking the add would time out every *arr and autobrr grab, since a
// magnet can take minutes to resolve.
func (s *Server) startMagnetResolve(target, mode, magnetURI, savePath, category string, trackers []string, cat *category) (string, error) {
	link, err := magnet.Parse(magnetURI)
	if err != nil {
		return "", err
	}
	// The magnet's own trackers first: they are the ones that know this swarm.
	all := append(append([]string{}, link.Trackers...), trackers...)

	src, err := s.metadataSourceFor(target, mode)
	if err != nil {
		return "", err
	}
	if _, err := src.FetchMetadata(link.InfoHash, all, nil, nil); err != nil {
		return "", fmt.Errorf("magnet: start resolution: %w", err)
	}
	magnetBegin(link.InfoHash, link.DisplayName, mode, target)
	go s.completeMagnetResolve(src, link, all, target, mode, savePath, category, cat)
	return link.InfoHash, nil
}

// completeMagnetResolve polls until the dict lands, then feeds a real .torrent
// back through the normal add path.
func (s *Server) completeMagnetResolve(src metadataSource, link *magnet.Link, trackers []string, target, mode, savePath, category string, cat *category) {
	deadline := time.Now().Add(magnetResolveTimeout)
	for time.Now().Before(deadline) {
		time.Sleep(magnetPollInterval)

		res, err := src.GetMetadata(link.InfoHash)
		if err != nil {
			magnetFailed(link.InfoHash, err)
			return
		}
		switch res.State {
		case "resolving":
			continue
		case "failed":
			magnetFailed(link.InfoHash, fmt.Errorf("%s", res.Error))
			return
		case "unknown":
			magnetFailed(link.InfoHash, fmt.Errorf("engine has no job for this magnet"))
			return
		case "done":
			if err := s.addResolvedMagnet(link, res.Info, trackers, target, mode, savePath, category, cat); err != nil {
				magnetFailed(link.InfoHash, err)
				return
			}
			magnetResolved(link.InfoHash)
			return
		default:
			magnetFailed(link.InfoHash, fmt.Errorf("unexpected resolution state %q", res.State))
			return
		}
	}
	magnetFailed(link.InfoHash, fmt.Errorf("resolution timed out after %s", magnetResolveTimeout))
}

// addResolvedMagnet turns the fetched dict into a .torrent and adds it exactly
// like any other .torrent -- same placement, same save_path rules, same shim
// behaviour. That reuse is the whole design: no second add path to keep in sync.
func (s *Server) addResolvedMagnet(link *magnet.Link, hexInfo string, trackers []string, target, mode, savePath, category string, cat *category) error {
	dict, err := hex.DecodeString(hexInfo)
	if err != nil {
		return fmt.Errorf("magnet: decode info dict: %w", err)
	}
	// The engine verified this already; doing it again costs a SHA-1 and keeps
	// a mismatched dict from ever reaching the disk.
	if err := magnet.Verify(dict, link.InfoHash); err != nil {
		return err
	}
	data, err := magnet.BuildTorrent(dict, trackers)
	if err != nil {
		return err
	}

	dir := filepath.Join(s.config.Daemon.DataDir, "uploads")
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("magnet: uploads dir: %w", err)
	}
	path := filepath.Join(dir, link.InfoHash+".torrent")
	if err := os.WriteFile(path, data, 0644); err != nil {
		return fmt.Errorf("magnet: write torrent: %w", err)
	}

	if _, err := s.routeAdd(target, mode, path, "", savePath, category, trackers, cat); err != nil {
		return fmt.Errorf("magnet: add resolved torrent: %w", err)
	}
	return nil
}
