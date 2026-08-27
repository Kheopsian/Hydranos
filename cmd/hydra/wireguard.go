package main

import (
	"context"
	"fmt"
	"log/slog"
	"net/netip"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/portfwd"
	"github.com/Kheopsian/hydra/internal/wgtun"
)

// The WireGuard supervisor: it brings each engine's tunnel up BEFORE that
// engine starts, and keeps the forwarded port fresh for as long as it runs.
//
// The ordering is the design. An engine that starts first would bind a port
// the tunnel has not been granted, announce it, and hand every tracker an
// address that answers nobody for a full announce cycle -- trackers keep what
// they were told. So the tunnel comes up, the port is obtained, the engine is
// born already correct, and nothing has to be walked back.
//
// That is also why this does not use the startup gate the gluetun follower
// needs. The gate exists to hold announces while a port is negotiated after
// boot. Here there is nothing to hold: the negotiation happened before the
// engine existed.

// tunnelWaitTimeout bounds the boot delay per tunnel. Long enough for a
// handshake across a bad link, short enough that a dead provider does not turn
// into a daemon that never starts.
const tunnelWaitTimeout = 25 * time.Second

type wgEngine struct {
	engineID string
	device   string
	provider wgtun.Provider
	cfg      *config.WireGuardConfig
	gateway  string
	// addrs is the tunnel's own address, kept because the NAT-PMP gateway is
	// derived from it on every renewal, not just the first.
	addrs []netip.Prefix
	// port and lease are the current mapping, guarded by the supervisor lock.
	port  int
	lease time.Duration
	note  string
}

type wgSupervisor struct {
	mgr     *wgtun.Manager
	dataDir string

	mu      sync.RWMutex
	engines map[string]*wgEngine
	order   []string
}

// portSetter is the half of an engine this supervisor needs once the daemon is
// running: the ability to move its listener when the lease rotates.
type portSetter interface{ SetListenPort(port int) error }

// startWireGuard brings up every tunnel the config asks for, and rewrites the
// engine configs it touched: bind_interface becomes the tunnel device, and
// listen_port becomes the forwarded port when the provider grants one.
//
// Returns nil when nothing asked for a tunnel, which is the common case and
// must cost nothing.
func startWireGuard(ctx context.Context, cfg *config.HydraConfig, engineCfgs []config.EngineConfig) *wgSupervisor {
	wanted := 0
	for i := range engineCfgs {
		if w := engineCfgs[i].SessionConfig.WireGuard; w != nil && w.Enabled {
			wanted++
		}
	}
	if wanted == 0 {
		return nil
	}
	if !wgtun.Supported() {
		// Loud, and not fatal. The alternative -- refusing to start -- would
		// strand a Windows agent whose config was pushed from a Linux front.
		slog.Error("wireguard: this platform cannot manage tunnels; the engines that asked for one will run WITHOUT it",
			"engines", wanted)
		return nil
	}
	sup := &wgSupervisor{
		mgr:     wgtun.NewManager(config.WireGuardDir(cfg.Daemon.DataDir)),
		dataDir: cfg.Daemon.DataDir,
		engines: map[string]*wgEngine{},
	}
	if err := sup.mgr.Preflight(ctx); err != nil {
		// Fail closed, and say exactly what to change. Starting anyway would
		// run every one of those engines on the host's own address -- the
		// leak the tunnel was configured to prevent, arriving silently at
		// boot.
		slog.Error("wireguard: cannot manage tunnels, refusing to start engines that asked for one", "error", err)
		os.Exit(1)
	}

	var wg sync.WaitGroup
	for i := range engineCfgs {
		ec := &engineCfgs[i]
		w := ec.SessionConfig.WireGuard
		if w == nil || !w.Enabled {
			continue
		}
		wg.Add(1)
		go func(idx int, ec *config.EngineConfig, w *config.WireGuardConfig) {
			defer wg.Done()
			if err := sup.bring(ctx, idx, ec, w); err != nil {
				// One tunnel failing must not take the daemon down: the other
				// engines are unaffected, and an engine whose device exists
				// but carries nothing fails closed rather than leaking.
				slog.Error("wireguard: tunnel not usable", "engine", ec.ID, "error", err)
			}
		}(i, ec, w)
	}
	wg.Wait()
	return sup
}

// bring is one engine's tunnel, end to end.
func (s *wgSupervisor) bring(ctx context.Context, idx int, ec *config.EngineConfig, w *config.WireGuardConfig) error {
	if err := w.Validate(); err != nil {
		return err
	}
	path := w.ConfigPath(s.dataDir)
	raw, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("reading %s: %w", path, err)
	}
	conf, err := wgtun.ParseConf(string(raw))
	if err != nil {
		return fmt.Errorf("%s: %w", path, err)
	}
	device := strings.TrimSpace(w.Device)
	if device == "" {
		device = wgtun.DeviceNameFor(ec.ID)
	}
	table := w.RouteTable
	if table == 0 {
		table = wgtun.TableFor(idx)
	}
	spec := wgtun.Spec{
		Device:       device,
		Table:        table,
		RulePriority: wgtun.RulePriorityFor(idx),
		Conf:         conf,
	}
	prov, known := wgtun.LookupProvider(w.Provider)
	if !known && strings.TrimSpace(w.Provider) != "" {
		slog.Warn("wireguard: unknown provider, no port will be requested",
			"engine", ec.ID, "provider", w.Provider)
	}

	e := &wgEngine{engineID: ec.ID, device: device, provider: prov, cfg: w, addrs: conf.Addresses}
	s.mu.Lock()
	s.engines[ec.ID] = e
	s.order = append(s.order, ec.ID)
	s.mu.Unlock()

	// The interface is what the engine pins to. Set here, from the tunnel we
	// actually created, rather than trusted from the file: a hand-written
	// bind_interface next to an enabled tunnel is two writers for one
	// decision, and they drift.
	if prev := strings.TrimSpace(ec.SessionConfig.BindInterface); prev != "" && prev != device {
		slog.Warn("wireguard: bind_interface is derived from the tunnel and overrides what the config says",
			"engine", ec.ID, "configured", prev, "using", device)
	}
	ec.SessionConfig.BindInterface = device

	if err := s.mgr.Up(ctx, ec.ID, prov.ID, spec); err != nil {
		return err
	}
	if err := s.mgr.WaitHandshake(ctx, device, tunnelWaitTimeout); err != nil {
		// Not fatal for this engine: WireGuard retries forever, and the device
		// stays up. The engine will be unreachable until the peer answers,
		// which is visible in the Network tab and in this line.
		s.note(ec.ID, fmt.Sprintf("no handshake yet: %v", err))
		slog.Error("wireguard: the tunnel is up but the peer has not answered",
			"engine", ec.ID, "device", device, "error", err)
		return nil
	}
	slog.Info("wireguard: tunnel ready", "engine", ec.ID, "device", device, "provider", prov.ID)

	port, err := s.acquire(ctx, e, 0)
	if err != nil {
		s.note(ec.ID, err.Error())
		slog.Error("wireguard: no forwarded port; this engine will take no incoming peers",
			"engine", ec.ID, "device", device, "error", err)
		return nil
	}
	if port > 0 {
		// Written into the config BEFORE the engine starts, which is the whole
		// point of doing this here: the engine binds the right port on its
		// first breath and announces it once.
		ec.SessionConfig.ListenPort = port
		slog.Info("wireguard: engine will listen on the forwarded port",
			"engine", ec.ID, "device", device, "port", port)
	}
	return nil
}

// acquire gets a port according to the provider's rules. Returns 0 with no
// error when the provider forwards nothing -- a legitimate, if unhappy, state.
func (s *wgSupervisor) acquire(ctx context.Context, e *wgEngine, suggested int) (int, error) {
	kind := e.provider.PortForward
	switch strings.ToLower(strings.TrimSpace(e.cfg.PortForward)) {
	case config.PortForwardAuto:
		kind = wgtun.PortForwardNATPMP
	case config.PortForwardManual:
		kind = wgtun.PortForwardManual
	case config.PortForwardOff:
		kind = wgtun.PortForwardNone
	}
	switch kind {
	case wgtun.PortForwardManual:
		if e.cfg.ManualPort > 0 {
			s.setPort(e.engineID, e.cfg.ManualPort, 0, "using the port set by hand")
			return e.cfg.ManualPort, nil
		}
		s.note(e.engineID, e.provider.Note)
		return 0, nil
	case wgtun.PortForwardNone:
		s.note(e.engineID, e.provider.Note)
		return 0, nil
	}
	gw, err := wgtun.Gateway(e.addrs)
	if err != nil {
		return 0, err
	}
	e.gateway = gw.String()
	n := portfwd.NATPMP{Gateway: gw, Device: e.device}
	port, lease, err := n.Acquire(ctx, 0, suggested)
	if err != nil {
		return 0, fmt.Errorf("asking %s for a forwarded port: %w", gw, err)
	}
	s.setPort(e.engineID, port, lease, fmt.Sprintf("forwarded port %d, renewed every %s", port, portfwd.RenewInterval(lease)))
	return port, nil
}

func (s *wgSupervisor) setPort(engineID string, port int, lease time.Duration, note string) {
	s.mu.Lock()
	if e := s.engines[engineID]; e != nil {
		e.port, e.lease, e.note = port, lease, note
	}
	s.mu.Unlock()
}

func (s *wgSupervisor) note(engineID, msg string) {
	s.mu.Lock()
	if e := s.engines[engineID]; e != nil {
		e.note = msg
	}
	s.mu.Unlock()
}

// StartPortFollowers keeps every automatic mapping alive, and moves the engine
// when the provider hands back a different number.
//
// A NAT-PMP lease is sixty seconds. Nothing about its expiry is reported: the
// port simply stops answering, the engine keeps announcing it, and the swarm
// drifts away over the following hour. So the renewal is not a background
// nicety, it is what keeps the node reachable at all.
func (s *wgSupervisor) StartPortFollowers(ctx context.Context, resolve func(engineID string) portSetter) {
	if s == nil {
		return
	}
	s.mu.RLock()
	ids := append([]string(nil), s.order...)
	s.mu.RUnlock()
	for _, id := range ids {
		s.mu.RLock()
		e := s.engines[id]
		s.mu.RUnlock()
		if e == nil || e.lease <= 0 {
			// No lease means nothing to renew: a manual port, or none at all.
			continue
		}
		go s.follow(ctx, e, resolve)
	}
}

// retryAfterFailure is how long to wait before asking again when a renewal
// did not get through.
//
// Not the usual half-lease: measured on a live Proton tunnel, the gateway goes
// quiet for a few seconds at a time while the tunnel itself keeps carrying
// traffic. Waiting another thirty seconds after such a blip spends most of the
// remaining lease doing nothing, and two blips in a row would drop the mapping
// -- at which point the engine announces a port that answers nobody and no
// error is ever produced, by anyone.
const retryAfterFailure = 5 * time.Second

func (s *wgSupervisor) follow(ctx context.Context, e *wgEngine, resolve func(string) portSetter) {
	failures := 0
	for {
		s.mu.RLock()
		lease, cur := e.lease, e.port
		s.mu.RUnlock()
		wait := portfwd.RenewInterval(lease)
		if failures > 0 {
			wait = retryAfterFailure
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(wait):
		}
		port, err := s.acquire(ctx, e, cur)
		if err != nil {
			failures++
			s.note(e.engineID, fmt.Sprintf("could not renew the forwarded port (attempt %d): %v", failures, err))
			// One miss is the tunnel breathing; several in a row means the
			// mapping is about to lapse, which is the thing nothing else
			// reports.
			if failures >= 3 {
				slog.Error("wireguard: the forwarded port has not been renewed and the lease is lapsing",
					"engine", e.engineID, "attempts", failures, "error", err)
			} else {
				slog.Warn("wireguard: port renewal failed, retrying shortly",
					"engine", e.engineID, "retry_in", retryAfterFailure.String(), "error", err)
			}
			continue
		}
		if failures > 0 {
			slog.Info("wireguard: the forwarded port was renewed again",
				"engine", e.engineID, "after_failures", failures, "port", port)
			failures = 0
		}
		if port == cur || port == 0 {
			continue
		}
		setter := resolve(e.engineID)
		if setter == nil {
			slog.Error("wireguard: the forwarded port changed but the engine cannot be reached to rebind",
				"engine", e.engineID, "from", cur, "to", port)
			continue
		}
		if err := setter.SetListenPort(port); err != nil {
			s.note(e.engineID, fmt.Sprintf("the forwarded port moved to %d but the rebind failed: %v", port, err))
			slog.Error("wireguard: rebind after a port change failed", "engine", e.engineID, "port", port, "error", err)
			continue
		}
		slog.Info("wireguard: forwarded port changed, engine rebound",
			"engine", e.engineID, "from", cur, "to", port)
	}
}

// Stop tears every tunnel down. Called on shutdown: a tunnel left behind keeps
// its routing rule, and the next start finds the priority taken by an
// interface that no longer exists.
func (s *wgSupervisor) Stop(ctx context.Context) {
	if s == nil {
		return
	}
	s.mgr.DownAll(ctx)
}

// States is what the API serves. It carries no key and no config text: the
// only secret in this feature stays in a 0600 file on the node that uses it.
func (s *wgSupervisor) States(ctx context.Context) []wgtun.TunnelState {
	if s == nil {
		return nil
	}
	byDevice := map[string]wgtun.State{}
	for _, st := range s.mgr.States(ctx) {
		byDevice[st.Device] = st
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := make([]wgtun.TunnelState, 0, len(s.order))
	for _, id := range s.order {
		e := s.engines[id]
		if e == nil {
			continue
		}
		st := byDevice[e.device]
		st.Engine = id
		out = append(out, wgtun.TunnelState{
			State:         st,
			Provider:      e.provider.ID,
			ProviderLabel: e.provider.Label,
			ForwardedPort: e.port,
			PortForward:   string(e.provider.PortForward),
			Note:          e.note,
		})
	}
	return out
}
