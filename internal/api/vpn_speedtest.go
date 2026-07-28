package api

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os/exec"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
)

// runIperf3 runs iperf3 upload then download. When a SOCKS5 proxy is
// configured (see SetSocks5Proxy), the TCP streams are routed through a
// local forwarder so the measurement reflects the real path torrents take
// (Hydra → SOCKS5 v6 → VPS gost → public iperf3 target) instead of a
// loopback measurement that overstates peer-facing capacity.
//
// Public iperf3 endpoints (ex. ping.online.net) listen on a port range and
// reject extra clients with "the server is busy". We retry across
// basePort..basePort+8 until one slot succeeds.
func runIperf3(server string, basePort, duration int) (map[string]interface{}, error) {
	var lastErr error
	for offset := 0; offset < 9; offset++ {
		port := basePort + offset
		res, err := runIperf3OnPort(server, port, duration)
		if err == nil && (res["ul_mbps"].(float64) > 0 || res["dl_mbps"].(float64) > 0) {
			return res, nil
		}
		if err != nil {
			lastErr = err
		}
	}
	if lastErr != nil {
		return nil, fmt.Errorf("vpn_speedtest: all ports failed: %w", lastErr)
	}
	return nil, fmt.Errorf("vpn_speedtest: all ports returned 0")
}

func runIperf3OnPort(server string, port, duration int) (map[string]interface{}, error) {
	targetHost, targetPort := server, port
	if proxy := getSocks5Proxy(); proxy != nil {
		fwd, err := newSocks5Forwarder(proxy, fmt.Sprintf("%s:%d", server, port))
		if err != nil {
			return nil, fmt.Errorf("vpn_speedtest forwarder: %w", err)
		}
		defer fwd.Close()
		targetHost = "127.0.0.1"
		targetPort = fwd.LocalPort()
	}

	ul, err := runIperf3Test(targetHost, targetPort, duration, false)
	if err != nil {
		slog.Debug("iperf3 upload error", "port", port, "error", err)
	}
	dl, err2 := runIperf3Test(targetHost, targetPort, duration, true)
	if err2 != nil {
		slog.Debug("iperf3 download error", "port", port, "error", err2)
	}
	if err != nil && err2 != nil {
		return nil, fmt.Errorf("iperf3 failed: ul=%v dl=%v", err, err2)
	}
	return map[string]interface{}{
		"ul_mbps": ul,
		"dl_mbps": dl,
		"ts":      float64(time.Now().Unix()),
	}, nil
}

func runIperf3Test(server string, port, duration int, reverse bool) (float64, error) {
	args := []string{"-c", server, "-p", fmt.Sprintf("%d", port), "-t", fmt.Sprintf("%d", duration), "-P", "4", "-J"}
	if reverse {
		args = append(args, "-R")
	}

	ctx, cancel := context.WithTimeout(context.Background(), time.Duration(duration+30)*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, "iperf3", args...)
	out, err := cmd.Output()
	if err != nil {
		return 0, err
	}

	var data struct {
		End struct {
			SumSent struct {
				BitsPerSecond float64 `json:"bits_per_second"`
			} `json:"sum_sent"`
			SumReceived struct {
				BitsPerSecond float64 `json:"bits_per_second"`
			} `json:"sum_received"`
		} `json:"end"`
	}
	if err := json.Unmarshal(out, &data); err != nil {
		return 0, err
	}

	bits := data.End.SumReceived.BitsPerSecond
	if bits == 0 {
		bits = data.End.SumSent.BitsPerSecond
	}
	mbps := bits / 1e6
	// Round to 1 decimal
	return float64(int(mbps*10+0.5)) / 10, nil
}

// StartVpnSpeedtestLoop launches the periodic VPN speedtest collector.
func StartVpnSpeedtestLoop(ctx context.Context, cfg config.VpnSpeedtestConfig, benchDB BenchDB) {
	if !cfg.Enabled || cfg.Iperf3Server == "" {
		slog.Info("vpn_speedtest: disabled or no server configured")
		return
	}

	interval := time.Duration(cfg.IntervalSecs) * time.Second
	if interval <= 0 {
		interval = time.Hour
	}

	go func() {
		// Wait 60s for daemon to stabilize
		select {
		case <-ctx.Done():
			return
		case <-time.After(60 * time.Second):
		}

		slog.Info("vpn_speedtest: collector started",
			"server", cfg.Iperf3Server, "port", cfg.Iperf3Port,
			"interval", interval, "duration", cfg.DurationSecs,
			"via_socks5", getSocks5Proxy() != nil)

		ticker := time.NewTicker(interval)
		defer ticker.Stop()

		// Run immediately on first iteration
		runAndStore(cfg, benchDB)

		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				runAndStore(cfg, benchDB)
			}
		}
	}()
}

func runAndStore(cfg config.VpnSpeedtestConfig, benchDB BenchDB) {
	result, err := runIperf3(cfg.Iperf3Server, cfg.Iperf3Port, cfg.DurationSecs)
	if err != nil {
		slog.Warn("vpn_speedtest: test failed", "error", err)
		return
	}
	slog.Info("vpn_speedtest: completed",
		"ul_mbps", result["ul_mbps"], "dl_mbps", result["dl_mbps"])
	if benchDB != nil {
		benchDB.InsertVpn(result["ts"].(float64), result["ul_mbps"].(float64), result["dl_mbps"].(float64))
	}
}

// --- SOCKS5 TCP forwarder --------------------------------------------------
//
// iperf3 speaks vanilla TCP to a fixed host:port. It has no SOCKS5 support,
// so we open a local listener, hand iperf3 that address, and tunnel every
// accepted connection through the SOCKS5 proxy to the real remote target.

type socks5Forwarder struct {
	ln        net.Listener
	dialer    *SOCKS5Dialer
	target    string
	closeOnce sync.Once
}

func newSocks5Forwarder(d *SOCKS5Dialer, target string) (*socks5Forwarder, error) {
	ln, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	f := &socks5Forwarder{ln: ln, dialer: d, target: target}
	go f.serve()
	return f, nil
}

func (f *socks5Forwarder) LocalPort() int {
	return f.ln.Addr().(*net.TCPAddr).Port
}

func (f *socks5Forwarder) Close() {
	f.closeOnce.Do(func() { _ = f.ln.Close() })
}

func (f *socks5Forwarder) serve() {
	for {
		conn, err := f.ln.Accept()
		if err != nil {
			return
		}
		go f.handle(conn)
	}
}

func (f *socks5Forwarder) handle(local net.Conn) {
	defer local.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	remote, err := f.dialer.Dial(ctx, f.target)
	cancel()
	if err != nil {
		slog.Warn("vpn_speedtest socks5 dial", "target", f.target, "error", err)
		return
	}
	defer remote.Close()
	done := make(chan struct{}, 2)
	go func() { _, _ = io.Copy(remote, local); done <- struct{}{} }()
	go func() { _, _ = io.Copy(local, remote); done <- struct{}{} }()
	<-done
}
