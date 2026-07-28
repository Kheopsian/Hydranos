package engine

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/version"
)

// EngineProcess manages a BitTorrent engine child process (either the Typhon
// Rust engine or patched rqbit). The concrete engine is chosen per session
// via SessionConfig.Engine. All callers interact with `.Client()` which
// returns a uniform EngineClient interface.
type EngineProcess struct {
	engine     string // "typhon" | "rqbit"
	cmd        *exec.Cmd
	socketPath string // typhon only
	httpAddr   string // rqbit only
	client     EngineClient
}

// EngineBinding is the JSON-serialised counterpart of Rust's
// `crate::config::Binding`. Mirrors the Go-side `Binding` struct
// (binding.go) but with JSON tags matching Typhon's serde fields.
type EngineBinding struct {
	ID         uint32 `json:"id"`
	PeerID     string `json:"peer_id,omitempty"`
	ListenAddr string `json:"listen_addr"`
	ListenPort uint16 `json:"listen_port"`
	// AnnouncePort = publicly-reachable port (NAT-PMP external on Proton WG).
	// Used for the BEP-10 `p` field. 0 means "fall back to listen_port"
	// (legacy single-binding without NAT translation).
	AnnouncePort uint16 `json:"announce_port,omitempty"`
	PublicIP     string `json:"public_ip,omitempty"`
	// Fwmark is applied via SO_MARK on Typhon's outbound dial sockets so
	// they route through the matching WG interface (matched by
	// `ip rule fwmark X lookup tableX`).
	Fwmark uint32 `json:"fwmark,omitempty"`
}

// EngineConfig is the JSON config passed to hydra-engine.
type EngineConfig struct {
	DataDir            string `json:"data_dir"`
	ResumeDir          string `json:"resume_dir"`
	ListenPort         int    `json:"listen_port"`
	ListenInterfaces   string `json:"listen_interfaces,omitempty"`
	OutgoingInterfaces string `json:"outgoing_interfaces,omitempty"`
	// Optional extra TCP listener expecting HAProxy PROXY protocol v2.
	// Set from SessionConfig.ListenPortProxyV2 (0 = disabled). Enables the
	// v6 bypass path : peer → VPS haproxy → the router rdr → the seedbox host [::]:N.
	ListenPortProxyV2 int `json:"listen_port_proxy_v2,omitempty"`
	// Explicit bind addr for PROXY v2 listener. If unset, defaults to [::].
	ListenAddrProxyV2     string   `json:"listen_addr_proxy_v2,omitempty"`
	ProxyV2TrustedSources []string `json:"proxy_v2_trusted_sources,omitempty"`
	Socks5OutboundHost    string   `json:"socks5_outbound_host,omitempty"`
	Socks5OutboundPort    int      `json:"socks5_outbound_port,omitempty"`
	Socks5OutboundUser    string   `json:"socks5_outbound_user,omitempty"`
	Socks5OutboundPass    string   `json:"socks5_outbound_pass,omitempty"`

	// Connection limits
	MaxConnections           int `json:"max_connections"`
	MaxConnectionsPerTorrent int `json:"max_connections_per_torrent"`
	MaxUploadsPerTorrent     int `json:"max_uploads_per_torrent"`

	// Choking
	UnchokeSlots         int    `json:"unchoke_slots"`
	OptimisticUnchoke    int    `json:"optimistic_unchoke"`
	UnchokeInterval      int    `json:"unchoke_interval"`
	ChokingAlgorithm     string `json:"choking_algorithm"`
	SeedChokingAlgorithm string `json:"seed_choking_algorithm"`

	// Send buffers
	SendBufferWatermark       int `json:"send_buffer_watermark"`
	SendBufferLowWatermark    int `json:"send_buffer_low_watermark"`
	SendBufferWatermarkFactor int `json:"send_buffer_watermark_factor"`

	// Timeouts
	RequestTimeout    int `json:"request_timeout"`
	PeerConnTimeout   int `json:"peer_connect_timeout"`
	HandshakeTimeout  int `json:"handshake_timeout"`
	PeerTimeout       int `json:"peer_timeout"`
	InactivityTimeout int `json:"inactivity_timeout"`

	// Peer management
	ConnectionSpeed      int `json:"connection_speed"`
	PeerTurnover         int `json:"peer_turnover"`
	PeerTurnoverInterval int `json:"peer_turnover_interval"`
	PeerTurnoverCutoff   int `json:"peer_turnover_cutoff"`
	AllowedFastSetSize   int `json:"allowed_fast_set_size"`

	// Features
	DHTEnabled    bool   `json:"dht_enabled"`
	PEXEnabled    bool   `json:"pex_enabled"`
	SuggestMode   int    `json:"suggest_mode"`
	MixedModeAlgo string `json:"mixed_mode_algorithm"`

	// Advanced
	MaxAllowedInRequestQueue int `json:"max_allowed_in_request_queue"`
	SendNotSentLowWatermark  int `json:"send_not_sent_low_watermark"`

	// Race-specific tuning
	WholePiecesThreshold int `json:"whole_pieces_threshold,omitempty"`
	MaxOutRequestQueue   int `json:"max_out_request_queue,omitempty"`
	RequestQueueTime     int `json:"request_queue_time,omitempty"`
	MaxRejects           int `json:"max_rejects,omitempty"`
	MaxFailcount         int `json:"max_failcount,omitempty"`

	// Identity
	PeerFingerprint string `json:"peer_fingerprint"`
	UserAgent       string `json:"user_agent"`

	// When true, Typhon's internal HTTP tracker announce loop is skipped.
	// Used post-multi-binding migration where the Go orchestrator owns
	// announces. Dial queue (PEX/DHT outbound) stays wired up.
	DisableInternalAnnounce bool `json:"disable_internal_announce,omitempty"`

	// Per-tunnel network bindings forwarded to Typhon. Empty = legacy mode
	// (Typhon falls back to ListenInterfaces / ListenPort / PeerFingerprint).
	// Populated when the multi-tunnel WG infra is wired up — each entry
	// produces one TCP listener with its own peer_id on the Rust side.
	Bindings []EngineBinding `json:"bindings,omitempty"`

	// I/O
	AIOThreads      int `json:"aio_threads"`
	FilePoolSize    int `json:"file_pool_size"`
	CacheSizeBlocks int `json:"cache_size_blocks"`
	CacheExpiry     int `json:"cache_expiry"`

	// Rate limits
	UploadLimit   int `json:"upload_limit"`
	DownloadLimit int `json:"download_limit"`
}

// BuildHoardConfig generates a hydra-engine config JSON from the Hydra session config.
func BuildHoardConfig(cfg *config.SessionConfig, dataDir string) EngineConfig {
	ulLimit := 0
	if cfg.UploadRateLimit > 0 {
		ulLimit = cfg.UploadRateLimit
	}
	maxConn := cfg.MaxConnections
	if maxConn <= 0 {
		maxConn = 8000
	}
	unchokeSlots := 50

	ec := EngineConfig{
		DataDir:                  dataDir,
		ResumeDir:                dataDir + "/resume",
		ListenPort:               cfg.ListenPort,
		ListenPortProxyV2:        cfg.ListenPortProxyV2,
		ListenAddrProxyV2:        cfg.ListenAddrProxyV2,
		ProxyV2TrustedSources:    cfg.ProxyV2TrustedSources,
		Socks5OutboundHost:       cfg.Socks5OutboundHost,
		Socks5OutboundPort:       cfg.Socks5OutboundPort,
		Socks5OutboundUser:       cfg.Socks5OutboundUser,
		Socks5OutboundPass:       cfg.Socks5OutboundPass,
		MaxConnections:           maxConn,
		MaxConnectionsPerTorrent: 50,
		MaxUploadsPerTorrent:     cfg.MaxUploadsPerTorrent, // -1 = unlimited

		// Choking
		UnchokeSlots:         unchokeSlots,
		OptimisticUnchoke:    0,
		UnchokeInterval:      15,
		ChokingAlgorithm:     "fixed_slots",
		SeedChokingAlgorithm: "round_robin",

		// Send buffers
		SendBufferWatermark:       33554432,
		SendBufferLowWatermark:    8388608,
		SendBufferWatermarkFactor: 200,

		// Timeouts
		RequestTimeout:    45,
		PeerConnTimeout:   10,
		HandshakeTimeout:  15,
		PeerTimeout:       valOrDefault(cfg.PeerTimeout, 300),
		InactivityTimeout: valOrDefault(cfg.InactivityTimeout, 300),

		// Peer management
		ConnectionSpeed:      500,
		PeerTurnover:         0,
		PeerTurnoverInterval: 120,
		PeerTurnoverCutoff:   90,
		AllowedFastSetSize:   0, // 0 = disabled

		// Features
		DHTEnabled:    true,
		PEXEnabled:    true,
		SuggestMode:   1, // suggest_read_cache
		MixedModeAlgo: "prefer_tcp",

		// Advanced
		MaxAllowedInRequestQueue: 2000,
		SendNotSentLowWatermark:  524288,

		// Identity
		PeerFingerprint: version.PeerFingerprint(),
		UserAgent:       version.UserAgent(),

		// Hydra Go orchestrator owns tracker announces (via HoardAnnouncer).
		// Typhon's internal announce loop is disabled; the dial queue
		// (PEX/DHT outbound) stays wired up.
		DisableInternalAnnounce: true,

		// I/O
		AIOThreads:      32,
		FilePoolSize:    5000,
		CacheSizeBlocks: 65536,
		CacheExpiry:     300,

		// Rate limits
		UploadLimit: ulLimit,
	}

	// Dual-tunnel ECMP: listen on both tunnel IPs if available.
	if cfg.ListenInterfaces != "" {
		ec.ListenInterfaces = cfg.ListenInterfaces
		parts := strings.Split(cfg.ListenInterfaces, ",")
		var ips []string
		for _, p := range parts {
			if idx := strings.LastIndex(p, ":"); idx > 0 {
				ips = append(ips, strings.TrimSpace(p[:idx]))
			}
		}
		if len(ips) > 1 {
			ec.OutgoingInterfaces = strings.Join(ips, ",")
		}
	}
	return ec
}

// BuildRaceConfig generates a hydra-engine config JSON for the race session.
func BuildRaceConfig(cfg *config.SessionConfig, dataDir string) EngineConfig {
	ulLimit := 0
	if cfg.UploadRateLimit > 0 {
		ulLimit = cfg.UploadRateLimit
	}
	maxConn := cfg.MaxConnections
	if maxConn <= 0 {
		maxConn = 4000
	}
	unchokeSlots := -1

	return EngineConfig{
		DataDir:                  dataDir,
		ResumeDir:                dataDir + "/resume",
		ListenPort:               cfg.ListenPort,
		ListenPortProxyV2:        cfg.ListenPortProxyV2,
		ListenAddrProxyV2:        cfg.ListenAddrProxyV2,
		ProxyV2TrustedSources:    cfg.ProxyV2TrustedSources,
		Socks5OutboundHost:       cfg.Socks5OutboundHost,
		Socks5OutboundPort:       cfg.Socks5OutboundPort,
		Socks5OutboundUser:       cfg.Socks5OutboundUser,
		Socks5OutboundPass:       cfg.Socks5OutboundPass,
		MaxConnections:           maxConn,
		MaxConnectionsPerTorrent: 1000,
		MaxUploadsPerTorrent:     valOrDefault(cfg.MaxUploadsPerTorrent, 100),

		// Choking
		UnchokeSlots:         unchokeSlots,
		OptimisticUnchoke:    3,
		UnchokeInterval:      3,
		ChokingAlgorithm:     "rate_based",
		SeedChokingAlgorithm: "anti_leech",

		// Send buffers
		SendBufferWatermark:       33554432,
		SendBufferLowWatermark:    1048576,
		SendBufferWatermarkFactor: 150,

		// Timeouts
		RequestTimeout:    10,
		PeerConnTimeout:   3,
		HandshakeTimeout:  5,
		PeerTimeout:       valOrDefault(cfg.PeerTimeout, 30),
		InactivityTimeout: valOrDefault(cfg.InactivityTimeout, 300),

		// Peer management
		ConnectionSpeed:      500,
		PeerTurnover:         8,
		PeerTurnoverInterval: 60,
		PeerTurnoverCutoff:   90,
		AllowedFastSetSize:   10,

		// Features
		DHTEnabled:  true,
		PEXEnabled:  true,
		SuggestMode: 1,

		// Race-specific tuning
		WholePiecesThreshold: 2,
		MaxOutRequestQueue:   1500,
		RequestQueueTime:     1,
		MaxRejects:           3,
		MaxFailcount:         1,
		MixedModeAlgo:        "prefer_tcp",

		// Advanced
		MaxAllowedInRequestQueue: 2000,
		SendNotSentLowWatermark:  524288,

		// Identity
		PeerFingerprint: version.PeerFingerprint(),
		UserAgent:       version.UserAgent(),

		// Hydra Go orchestrator owns tracker announces (via HoardAnnouncer).
		// Typhon's internal announce loop is disabled; the dial queue
		// (PEX/DHT outbound) stays wired up.
		DisableInternalAnnounce: true,

		// I/O
		AIOThreads:      16,
		FilePoolSize:    3000,
		CacheSizeBlocks: 32768,
		CacheExpiry:     120,

		// Rate limits
		UploadLimit: ulLimit,
	}
}

func valOrDefault(v, def int) int {
	if v != 0 {
		return v
	}
	return def
}

func strOrDefault(v, def string) string {
	if v != "" {
		return v
	}
	return def
}

// StartEngineProcess starts a hydra-engine child process and connects to its Unix socket.
func StartEngineProcess(engineCfg EngineConfig, socketPath string) (*EngineProcess, error) {
	configData, err := json.MarshalIndent(engineCfg, "", "  ")
	if err != nil {
		return nil, fmt.Errorf("marshal engine config: %w", err)
	}

	configPath := socketPath + ".json"
	if err := os.WriteFile(configPath, configData, 0644); err != nil {
		return nil, fmt.Errorf("write engine config: %w", err)
	}

	os.Remove(socketPath)

	cmd := exec.Command("/usr/local/bin/hydra-engine",
		"--config", configPath,
		"--socket", socketPath)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("start engine process: %w", err)
	}

	slog.Info("engine process started",
		"pid", cmd.Process.Pid,
		"socket", socketPath,
		"config", configPath,
	)

	deadline := time.Now().Add(300 * time.Second) // large hoards (60k+) load resume cold > 30s
	for time.Now().Before(deadline) {
		if _, err := os.Stat(socketPath); err == nil {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}

	if _, err := os.Stat(socketPath); err != nil {
		cmd.Process.Kill()
		return nil, fmt.Errorf("engine socket not created after 300s: %s", socketPath)
	}

	client, err := ltclient.Connect(socketPath)
	if err != nil {
		cmd.Process.Kill()
		return nil, fmt.Errorf("connect to engine: %w", err)
	}

	if err := client.Ping(); err != nil {
		client.Close()
		cmd.Process.Kill()
		return nil, fmt.Errorf("ping engine: %w", err)
	}

	slog.Info("engine process connected", "socket", socketPath)

	return &EngineProcess{
		cmd:        cmd,
		socketPath: socketPath,
		client:     client,
	}, nil
}

// Client returns the engine client interface (Typhon or rqbit).
func (ep *EngineProcess) Client() EngineClient {
	return ep.client
}

// Stop gracefully shuts down the engine process.
func (ep *EngineProcess) Stop() error {
	if ep.client != nil {
		ep.client.Close()
	}
	if ep.cmd != nil && ep.cmd.Process != nil {
		ep.cmd.Process.Signal(os.Interrupt)
		done := make(chan error, 1)
		go func() { done <- ep.cmd.Wait() }()
		select {
		case <-done:
		case <-time.After(10 * time.Second):
			ep.cmd.Process.Kill()
		}
	}
	return nil
}
