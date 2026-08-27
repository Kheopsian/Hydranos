package wgtun

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Runner executes one command. Swapped out in tests; in production it is
// exec.CommandContext.
type Runner interface {
	Run(ctx context.Context, args ...string) (string, error)
}

type execRunner struct{}

func (execRunner) Run(ctx context.Context, args ...string) (string, error) {
	cmd := exec.CommandContext(ctx, args[0], args[1:]...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return string(out), fmt.Errorf("%s: %w: %s", strings.Join(args, " "), err, strings.TrimSpace(string(out)))
	}
	return string(out), nil
}

// State is what a tunnel is doing, for the Network tab.
type State struct {
	Device       string    `json:"device"`
	Engine       string    `json:"engine"`
	Provider     string    `json:"provider"`
	Up           bool      `json:"up"`
	Handshake    time.Time `json:"handshake,omitempty"`
	Endpoint     string    `json:"endpoint,omitempty"`
	Address      string    `json:"address,omitempty"`
	Table        int       `json:"table"`
	RxBytes      int64     `json:"rx_bytes"`
	TxBytes      int64     `json:"tx_bytes"`
	LastError    string    `json:"last_error,omitempty"`
	ManagedBy    string    `json:"managed_by"`
	HandshakeAge float64   `json:"handshake_age_seconds,omitempty"`
}

// Manager owns every tunnel Hydra brought up in this process.
type Manager struct {
	runner Runner
	// confDir is where a tunnel's temporary `wg setconf` file is written. It
	// lives on the data dir rather than /tmp so that a host with a
	// world-readable /tmp, or none at all, does not decide where a private key
	// briefly lands.
	confDir string

	mu    sync.RWMutex
	live  map[string]*tunnel
	order []string
}

type tunnel struct {
	spec     Spec
	engine   string
	provider string
	lastErr  string
}

// ErrUnsupported is returned on platforms where this cannot work at all.
var ErrUnsupported = errors.New("native WireGuard tunnels are only supported on Linux")

// Supported says whether this build can manage tunnels.
//
// The Windows agent is the reason this is a function rather than an
// assumption: it runs the same binary and must degrade to "configure your
// tunnel yourself and name it in bind_interface" instead of failing to start.
func Supported() bool { return runtime.GOOS == "linux" }

func NewManager(confDir string) *Manager {
	return &Manager{runner: execRunner{}, confDir: confDir, live: map[string]*tunnel{}}
}

// SetRunner is for tests.
func (m *Manager) SetRunner(r Runner) { m.runner = r }

// Preflight checks the things whose absence produces a confusing failure
// later: the two binaries, and the capability.
//
// Worth its own step because the failure it prevents is nasty. Without
// CAP_NET_ADMIN, `ip link add` fails with "Operation not permitted" and
// nothing says which capability, so the operator reads it as a bug. The engine
// meanwhile is configured to pin to a device that will never exist, and pins
// fail closed -- so the visible symptom is an engine that announces nothing,
// several layers away from the cause.
func (m *Manager) Preflight(ctx context.Context) error {
	if !Supported() {
		return ErrUnsupported
	}
	for _, bin := range []string{"ip", "wg"} {
		if _, err := exec.LookPath(bin); err != nil {
			return fmt.Errorf("%q is not installed: native WireGuard needs iproute2 and wireguard-tools (the Hydra container ships both; a bare-metal install may not)", bin)
		}
	}
	// Probe rather than read /proc/self/status: a capability set says what we
	// are allowed to ask for, not whether the kernel has the wireguard module.
	// A name no engine can produce: DeviceNameFor prefixes hy- and an engine
	// id of "capcheck" would collide, so the probe uses a character an id
	// cannot contain after cleaning.
	const probe = "hy_capcheck"
	if _, err := m.runner.Run(ctx, "ip", "link", "add", "dev", probe, "type", "wireguard"); err != nil {
		if strings.Contains(err.Error(), "not permitted") {
			return fmt.Errorf("not allowed to create network interfaces: give the container NET_ADMIN (docker run --cap-add=NET_ADMIN) or run it privileged (%w)", err)
		}
		if strings.Contains(err.Error(), "not supported") || strings.Contains(err.Error(), "Unknown device type") {
			return fmt.Errorf("this kernel has no WireGuard support: load the wireguard module on the HOST, not in the container (%w)", err)
		}
		return err
	}
	_, _ = m.runner.Run(ctx, "ip", "link", "del", "dev", probe)
	return nil
}

// Up brings one tunnel up, or replaces it in place.
//
// On failure the device is deliberately LEFT BEHIND when it was created. It
// looks wrong -- the tidy reflex is to delete it -- but deleting it is what
// would cause a leak: the engine pins its sockets to that device by name, and
// a name that does not resolve is the one case where some code paths fall back
// to the default route. A device that exists and carries nothing fails closed:
// every socket bound to it gets ENETUNREACH, which is loud, local and safe.
func (m *Manager) Up(ctx context.Context, engineID, provider string, spec Spec) error {
	if !Supported() {
		return ErrUnsupported
	}
	steps, err := UpPlan(spec)
	if err != nil {
		return err
	}
	confPath, err := m.writeSetconf(spec)
	if err != nil {
		return err
	}
	defer os.Remove(confPath)

	m.mu.Lock()
	t := m.live[spec.Device]
	if t == nil {
		t = &tunnel{}
		m.live[spec.Device] = t
		m.order = append(m.order, spec.Device)
	}
	t.spec, t.engine, t.provider, t.lastErr = spec, engineID, provider, ""
	m.mu.Unlock()

	// A family that fails takes the rest of its own steps with it, and nothing
	// else. Without this the v6 route would be attempted after the v6 address
	// was refused, producing a second, more confusing error about the first.
	skipFamily := map[string]bool{}
	for _, st := range steps {
		if st.Family != "" && skipFamily[st.Family] {
			continue
		}
		if st.WriteFile != "" {
			if err := os.WriteFile(st.WriteFile, []byte(st.WriteData), 0o644); err != nil {
				if st.SoftFail {
					skipFamily[st.Family] = true
					m.note(spec.Device, fmt.Sprintf("IPv%s is not available on this tunnel (%s): %v", st.Family, st.Desc, err))
					slog.Warn("wireguard: tunnel is up without one address family",
						"engine", engineID, "device", spec.Device, "family", "IPv"+st.Family, "error", err)
					continue
				}
				m.note(spec.Device, err.Error())
				return fmt.Errorf("bringing up %s (%s): %w", spec.Device, st.Desc, err)
			}
			continue
		}
		args := make([]string, len(st.Args))
		copy(args, st.Args)
		for i := range args {
			if args[i] == ConfPathPlaceholder {
				args[i] = confPath
			}
		}
		if _, err := m.runner.Run(ctx, args...); err != nil {
			if st.IgnoreError {
				continue
			}
			if st.SoftFail {
				skipFamily[st.Family] = true
				msg := fmt.Sprintf("IPv%s is not available on this tunnel (%s): %v", st.Family, st.Desc, err)
				m.note(spec.Device, msg)
				slog.Warn("wireguard: tunnel is up without one address family",
					"engine", engineID, "device", spec.Device, "family", "IPv"+st.Family, "error", err)
				continue
			}
			m.note(spec.Device, err.Error())
			return fmt.Errorf("bringing up %s (%s): %w", spec.Device, st.Desc, err)
		}
	}
	slog.Info("wireguard: tunnel up",
		"engine", engineID, "device", spec.Device, "provider", provider, "table", spec.Table)
	return nil
}

// WaitHandshake blocks until the tunnel has completed a handshake, or the
// deadline passes.
//
// This is what the startup gate waits on. Announcing before the tunnel carries
// traffic would publish an address the peers cannot reach for a whole announce
// cycle, and trackers keep it.
func (m *Manager) WaitHandshake(ctx context.Context, device string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	var last error
	for {
		ts, err := m.latestHandshake(ctx, device)
		if err == nil && !ts.IsZero() {
			return nil
		}
		last = err
		if time.Now().After(deadline) {
			if last == nil {
				last = fmt.Errorf("no handshake after %s: check the peer endpoint and the key, and that the host can reach the endpoint's UDP port", timeout)
			}
			m.note(device, last.Error())
			return last
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(500 * time.Millisecond):
		}
	}
}

func (m *Manager) latestHandshake(ctx context.Context, device string) (time.Time, error) {
	out, err := m.runner.Run(ctx, "wg", "show", device, "latest-handshakes")
	if err != nil {
		return time.Time{}, err
	}
	var newest int64
	for _, line := range strings.Split(strings.TrimSpace(out), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		if n, err := strconv.ParseInt(fields[len(fields)-1], 10, 64); err == nil && n > newest {
			newest = n
		}
	}
	if newest == 0 {
		return time.Time{}, nil
	}
	return time.Unix(newest, 0), nil
}

// Down removes one tunnel.
func (m *Manager) Down(ctx context.Context, device string) error {
	m.mu.Lock()
	t := m.live[device]
	delete(m.live, device)
	for i, v := range m.order {
		if v == device {
			m.order = append(m.order[:i], m.order[i+1:]...)
			break
		}
	}
	m.mu.Unlock()
	if t == nil {
		return nil
	}
	for _, st := range DownPlan(t.spec) {
		if _, err := m.runner.Run(ctx, st.Args...); err != nil && !st.IgnoreError {
			return err
		}
	}
	slog.Info("wireguard: tunnel down", "device", device, "engine", t.engine)
	return nil
}

// DownAll tears every tunnel down, for a clean shutdown. A tunnel left behind
// keeps its rule, and the next start would find the priority taken.
func (m *Manager) DownAll(ctx context.Context) {
	m.mu.RLock()
	devs := append([]string(nil), m.order...)
	m.mu.RUnlock()
	for _, d := range devs {
		if err := m.Down(ctx, d); err != nil {
			slog.Warn("wireguard: could not remove tunnel", "device", d, "error", err)
		}
	}
}

func (m *Manager) note(device, msg string) {
	m.mu.Lock()
	if t := m.live[device]; t != nil {
		t.lastErr = msg
	}
	m.mu.Unlock()
}

// States reports every tunnel, for the API.
func (m *Manager) States(ctx context.Context) []State {
	m.mu.RLock()
	devs := append([]string(nil), m.order...)
	snap := make(map[string]tunnel, len(devs))
	for _, d := range devs {
		if t := m.live[d]; t != nil {
			snap[d] = *t
		}
	}
	m.mu.RUnlock()

	out := make([]State, 0, len(devs))
	for _, d := range devs {
		t := snap[d]
		st := State{
			Device: d, Engine: t.engine, Provider: t.provider,
			Table: t.spec.Table, LastError: t.lastErr, ManagedBy: "hydra",
		}
		if len(t.spec.Conf.Addresses) > 0 {
			st.Address = t.spec.Conf.Addresses[0].String()
		}
		if len(t.spec.Conf.Peers) > 0 {
			st.Endpoint = t.spec.Conf.Peers[0].Endpoint
		}
		if ts, err := m.latestHandshake(ctx, d); err == nil && !ts.IsZero() {
			st.Handshake = ts
			st.HandshakeAge = time.Since(ts).Seconds()
			// A WireGuard peer rehandshakes about every two minutes when there
			// is traffic. Anything older than five means the tunnel is not
			// carrying anything, whatever the interface flags say.
			st.Up = time.Since(ts) < 5*time.Minute
		}
		st.RxBytes, st.TxBytes = m.transfer(ctx, d)
		out = append(out, st)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Device < out[j].Device })
	return out
}

func (m *Manager) transfer(ctx context.Context, device string) (rx, tx int64) {
	out, err := m.runner.Run(ctx, "wg", "show", device, "transfer")
	if err != nil {
		return 0, 0
	}
	for _, line := range strings.Split(strings.TrimSpace(out), "\n") {
		f := strings.Fields(line)
		if len(f) < 3 {
			continue
		}
		r, _ := strconv.ParseInt(f[len(f)-2], 10, 64)
		t, _ := strconv.ParseInt(f[len(f)-1], 10, 64)
		rx += r
		tx += t
	}
	return rx, tx
}

// writeSetconf drops the `wg setconf` subset in a 0600 file.
//
// The private key touches the filesystem for the length of one command, which
// is unavoidable: `wg setconf` reads a file. What IS avoidable is where and
// with what mode, so: our own directory, 0600, created with O_EXCL, removed by
// the caller's defer even on the failure paths.
func (m *Manager) writeSetconf(spec Spec) (string, error) {
	dir := m.confDir
	if dir == "" {
		dir = os.TempDir()
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	path := filepath.Join(dir, fmt.Sprintf(".wg-%s-%d.conf", spec.Device, os.Getpid()))
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		if os.IsExist(err) {
			_ = os.Remove(path)
			f, err = os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
		}
		if err != nil {
			return "", err
		}
	}
	if _, err := f.WriteString(spec.Conf.SetconfText()); err != nil {
		f.Close()
		os.Remove(path)
		return "", err
	}
	if err := f.Close(); err != nil {
		os.Remove(path)
		return "", err
	}
	return path, nil
}
