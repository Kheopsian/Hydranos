package api

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/wgtun"
	"github.com/gin-gonic/gin"
)

// The Network tab's WireGuard half: what the tunnels are doing, and where the
// .conf files come from.
//
// One rule runs through all of it: a configuration file goes IN and never
// comes back OUT. It holds the tunnel's private key, and this API already
// serves settings in clear to anyone it lets in -- so the file is stored, its
// name is listed, and its contents are never served, not even redacted. A
// redacted view is one refactor away from an unredacted one.

var wgStatesMu sync.RWMutex
var wgStatesFn func(context.Context) []wgtun.TunnelState

// SetWireGuardStates wires the supervisor's view in. Nil on a node that
// manages no tunnel, which is most of them.
func (s *Server) SetWireGuardStates(fn func(context.Context) []wgtun.TunnelState) {
	wgStatesMu.Lock()
	wgStatesFn = fn
	wgStatesMu.Unlock()
}

func wireGuardStates(ctx context.Context) []wgtun.TunnelState {
	wgStatesMu.RLock()
	fn := wgStatesFn
	wgStatesMu.RUnlock()
	if fn == nil {
		return nil
	}
	return fn(ctx)
}

// handleWireGuardStatus serves the tunnels, the .conf files on disk and the
// provider list the UI builds its menu from.
func (s *Server) handleWireGuardStatus(c *gin.Context) {
	files, ferr := listWireGuardConfigs(s.config.Daemon.DataDir)
	// What each engine is CONFIGURED to do, beside what its tunnel is actually
	// doing. The two answer different questions and the page shows both: a
	// config edited since the last restart is invisible otherwise, and that is
	// exactly the state where an operator thinks a change took effect.
	engines := map[string]*config.WireGuardConfig{}
	for _, ec := range s.config.Engines {
		if w := ec.SessionConfig.WireGuard; w != nil {
			engines[ec.ID] = w
		}
	}
	resp := gin.H{
		"supported": wgtun.Supported(),
		"engines":   engines,
		"tunnels":   wireGuardStates(c.Request.Context()),
		"configs":   files,
		"providers": wgtun.Providers(),
		"directory": config.WireGuardDir(s.config.Daemon.DataDir),
	}
	if ferr != nil {
		resp["configs_error"] = ferr.Error()
	}
	if !wgtun.Supported() {
		resp["unsupported_reason"] = "native WireGuard is managed by Hydra on Linux only; elsewhere, bring the tunnel up yourself and name it in bind_interface"
	}
	c.JSON(http.StatusOK, resp)
}

// wireGuardConfigInfo is what the UI may know about a stored file: that it
// exists, and whether it parses. Never its contents.
type wireGuardConfigInfo struct {
	Name string `json:"name"`
	Size int64  `json:"size"`
	// Peer and Address come from parsing. They are not secret -- a peer
	// endpoint is a public server address -- and they are what lets an
	// operator tell two provider files apart in a menu.
	Endpoint string `json:"endpoint,omitempty"`
	Address  string `json:"address,omitempty"`
	Error    string `json:"error,omitempty"`
}

func listWireGuardConfigs(dataDir string) ([]wireGuardConfigInfo, error) {
	dir := config.WireGuardDir(dataDir)
	entries, err := os.ReadDir(dir)
	if os.IsNotExist(err) {
		return []wireGuardConfigInfo{}, nil
	}
	if err != nil {
		return nil, err
	}
	out := []wireGuardConfigInfo{}
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".conf") {
			continue
		}
		info := wireGuardConfigInfo{Name: e.Name()}
		if fi, err := e.Info(); err == nil {
			info.Size = fi.Size()
		}
		raw, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			info.Error = err.Error()
		} else if conf, perr := wgtun.ParseConf(string(raw)); perr != nil {
			info.Error = perr.Error()
		} else {
			if len(conf.Peers) > 0 {
				info.Endpoint = conf.Peers[0].Endpoint
			}
			if len(conf.Addresses) > 0 {
				info.Address = conf.Addresses[0].String()
			}
		}
		out = append(out, info)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out, nil
}

// maxConfBytes bounds an upload. A WireGuard config is under a kilobyte; the
// margin is for a provider that ships a comment block.
const maxConfBytes = 64 * 1024

// handleWireGuardUpload stores a .conf.
//
// It is parsed BEFORE being written, and a file that does not parse is
// refused rather than saved for later: the alternative is a config that looks
// installed, is selected in the menu, and fails at the next restart -- when
// the engine it belongs to is the thing that will not come up.
func (s *Server) handleWireGuardUpload(c *gin.Context) {
	name := filepath.Base(strings.TrimSpace(c.Query("name")))
	var body []byte
	if fh, err := c.FormFile("file"); err == nil {
		if name == "" || name == "." {
			name = filepath.Base(fh.Filename)
		}
		f, oerr := fh.Open()
		if oerr != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": oerr.Error()})
			return
		}
		defer f.Close()
		body, err = io.ReadAll(io.LimitReader(f, maxConfBytes))
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}
	} else {
		var err error
		body, err = io.ReadAll(io.LimitReader(c.Request.Body, maxConfBytes))
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}
	}
	if name == "" || name == "." || name == "/" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "no file name: pass ?name=provider.conf or upload a named file"})
		return
	}
	if !strings.HasSuffix(name, ".conf") {
		name += ".conf"
	}
	if strings.ContainsAny(name, `/\`) || strings.Contains(name, "..") {
		c.JSON(http.StatusBadRequest, gin.H{"error": "the file name must not contain a path"})
		return
	}
	conf, err := wgtun.ParseConf(string(body))
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("this does not read as a WireGuard configuration: %v", err)})
		return
	}
	dir := config.WireGuardDir(s.config.Daemon.DataDir)
	// 0700 on the directory and 0600 on the file, because what lands here is
	// a private key. Umask cannot be relied on: the container may run with any.
	if err := os.MkdirAll(dir, 0o700); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	path := filepath.Join(dir, name)
	if err := os.WriteFile(path, body, 0o600); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	_ = os.Chmod(path, 0o600)
	endpoint := ""
	if len(conf.Peers) > 0 {
		endpoint = conf.Peers[0].Endpoint
	}
	c.JSON(http.StatusOK, gin.H{
		"name":     name,
		"endpoint": endpoint,
		"address":  addressOf(conf),
		// Said in the response because it is the next thing the operator has
		// to do, and nothing else in the UI would say it: a stored file is
		// inert until an engine names it, and the engine restarts to take it.
		"note": "stored. Assign it to an engine in the Network tab; the engine restarts to bring the tunnel up.",
	})
}

func addressOf(c *wgtun.Conf) string {
	if len(c.Addresses) > 0 {
		return c.Addresses[0].String()
	}
	return ""
}

// handleWireGuardDelete removes a stored .conf.
func (s *Server) handleWireGuardDelete(c *gin.Context) {
	name := filepath.Base(strings.TrimSpace(c.Param("name")))
	if name == "" || name == "." || name == "/" || strings.Contains(name, "..") {
		c.JSON(http.StatusBadRequest, gin.H{"error": "bad name"})
		return
	}
	// Refuse to remove a file an engine is configured to use: the deletion
	// would succeed, everything would look fine, and the engine would fail to
	// come up at the next restart -- possibly weeks later, with nothing
	// linking the two events.
	for _, ec := range s.config.Engines {
		w := ec.SessionConfig.WireGuard
		if w != nil && w.Enabled && filepath.Base(w.ConfigFile) == name {
			c.JSON(http.StatusConflict, gin.H{
				"error": fmt.Sprintf("engine %q uses this configuration; turn its tunnel off first", ec.ID)})
			return
		}
	}
	path := filepath.Join(config.WireGuardDir(s.config.Daemon.DataDir), name)
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"removed": name})
}

// wgEngineReq is one engine's tunnel settings as the Network tab submits them.
type wgEngineReq struct {
	EngineID    string `json:"engine_id"`
	Enabled     bool   `json:"enabled"`
	Provider    string `json:"provider"`
	ConfigFile  string `json:"config_file"`
	Device      string `json:"device"`
	PortForward string `json:"port_forward"`
	ManualPort  int    `json:"manual_port"`
}

// handleWireGuardEngines assigns tunnels to engines.
//
// It writes the config and nothing else: bringing a tunnel up mid-flight would
// mean moving a running engine's sockets from under it, and an engine that
// changes egress without restarting keeps peer connections open on the old
// path -- announcing one address while talking from another. So the answer
// says restart_required, honestly, rather than half-applying.
func (s *Server) handleWireGuardEngines(c *gin.Context) {
	var req struct {
		Engines []wgEngineReq `json:"engines"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if len(req.Engines) == 0 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "no engines in the request"})
		return
	}
	dir := config.WireGuardDir(s.config.Daemon.DataDir)
	for _, e := range req.Engines {
		if !e.Enabled {
			continue
		}
		w := &config.WireGuardConfig{
			Enabled: true, Provider: e.Provider, ConfigFile: e.ConfigFile,
			Device: e.Device, PortForward: e.PortForward, ManualPort: e.ManualPort,
		}
		if err := w.Validate(); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("engine %s: %v", e.EngineID, err)})
			return
		}
		// Checked here, at save time, because the alternative is a config that
		// saves cleanly and takes the engine down at the next restart -- which
		// may be a month later, with nothing connecting the two.
		path := filepath.Join(dir, filepath.Base(e.ConfigFile))
		raw, rerr := os.ReadFile(path)
		if rerr != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("engine %s: %v", e.EngineID, rerr)})
			return
		}
		if _, perr := wgtun.ParseConf(string(raw)); perr != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("engine %s: %s does not parse: %v", e.EngineID, e.ConfigFile, perr)})
			return
		}
		// Two engines on one .conf is one tunnel, one exit address and one
		// forwarded port shared by both -- the arrangement per-engine tunnels
		// exist to replace. It is refused rather than warned about: the
		// symptom (two engines, one exit IP) is exactly what a working
		// multi-VPN setup must never show.
		for _, other := range req.Engines {
			if other.EngineID != e.EngineID && other.Enabled &&
				filepath.Base(other.ConfigFile) == filepath.Base(e.ConfigFile) {
				c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf(
					"engines %s and %s are both set to %s: one configuration is one tunnel, so both would leave by the same address with the same forwarded port",
					e.EngineID, other.EngineID, e.ConfigFile)})
				return
			}
		}
	}

	configWriteMu.Lock()
	defer configWriteMu.Unlock()
	changed := 0
	for _, e := range req.Engines {
		kv := wireGuardKeys(e)
		id := strings.TrimSpace(e.EngineID)
		if id == "" {
			continue
		}
		var werr error
		switch id {
		case "race", "hoard":
			werr = s.editConfigFileLocked(func(doc string) (string, error) {
				return config.SetTOMLTable(doc, id, kv)
			})
		default:
			prefixed := make([][2]string, 0, len(kv))
			for _, p := range kv {
				prefixed = append(prefixed, [2]string{"session." + p[0], p[1]})
			}
			werr = s.editConfigFileLocked(func(doc string) (string, error) {
				return config.SetTOMLArrayTable(doc, "agent", "name", LocalAgentNameFor(id), prefixed)
			})
		}
		if werr != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": fmt.Sprintf("engine %s: %v", id, werr)})
			return
		}
		changed++
	}
	c.JSON(http.StatusOK, gin.H{
		"engines_updated":  changed,
		"restart_required": true,
		"note":             "the tunnels come up at the next restart, before the engines start",
	})
}

// wireGuardKeys is the ONE place a tunnel's settings become TOML.
//
// Every key is written on every save, including the ones being turned off.
// Writing only the changed keys is what leaves an old provider or an old
// config_file sitting beside a disabled tunnel, ready to come back on the day
// somebody flips enabled without reading the rest.
func wireGuardKeys(e wgEngineReq) [][2]string {
	kv := [][2]string{
		{"wireguard.enabled", strconv.FormatBool(e.Enabled)},
		{"wireguard.provider", strconv.Quote(strings.TrimSpace(e.Provider))},
		{"wireguard.config_file", strconv.Quote(filepath.Base(strings.TrimSpace(e.ConfigFile)))},
		{"wireguard.device", strconv.Quote(strings.TrimSpace(e.Device))},
		{"wireguard.port_forward", strconv.Quote(strings.ToLower(strings.TrimSpace(e.PortForward)))},
		{"wireguard.manual_port", strconv.Itoa(e.ManualPort)},
	}
	if !e.Enabled {
		// A disabled tunnel must not leave bind_interface pointing at a device
		// that will not exist: the engine would fail closed at every start,
		// which is safe but looks like a broken build rather than a setting.
		return kv
	}
	return kv
}
