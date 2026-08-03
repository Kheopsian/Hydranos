package config

import (
	"os"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

// RaceChokingConfig — custom choking algorithm settings.
type RaceChokingConfig struct {
	Enabled             bool    `toml:"enabled"`
	TickIntervalSeconds float64 `toml:"tick_interval_seconds"`
	Strategy            string  `toml:"strategy"`
	MaxUnchoked         int     `toml:"max_unchoked"`
	RarityWeight        float64 `toml:"rarity_weight"`
	SpeedWeight         float64 `toml:"speed_weight"`
}

// SessionConfig — engine session parameters (race or hoard). Only fields the
// Typhon engine actually consumes are kept; libtorrent-era tuning knobs were
// removed (see git history — they were packed into the engine JSON but ignored).
type SessionConfig struct {
	ListenPort       int    `toml:"listen_port"`
	ListenInterfaces string `toml:"listen_interfaces"`
	BindInterface    string `toml:"bind_interface"` // interface NAME (e.g. "wg0"); resolved to its IP at engine start
	// Optional second TCP listener expecting HAProxy PROXY protocol v2 (real
	// peer IP in header). Used by the v6 bypass path: peer → VPS haproxy →
	// the router rdr → the seedbox host [::]:listen_port_proxy_v2. 0 = disabled.
	ListenPortProxyV2 int `toml:"listen_port_proxy_v2"`
	// Explicit bind addr for the PROXY v2 listener (e.g. "[2001:db8:abc:d12::2]").
	// Empty = [::] wildcard (risk of source-selection bug with multiple v6 IPs).
	ListenAddrProxyV2     string   `toml:"listen_addr_proxy_v2"`
	ProxyV2TrustedSources []string `toml:"proxy_v2_trusted_sources"`
	Socks5OutboundHost    string   `toml:"socks5_outbound_host"`
	Socks5OutboundPort    int      `toml:"socks5_outbound_port"`
	Socks5OutboundUser    string   `toml:"socks5_outbound_user"`
	Socks5OutboundPass    string   `toml:"socks5_outbound_pass"`
	MaxConnections        int      `toml:"max_connections"`
	MaxUploadsPerTorrent  int      `toml:"max_uploads_per_torrent"`

	PeerTimeout       int `toml:"peer_timeout"`
	InactivityTimeout int `toml:"inactivity_timeout"`

	ActiveSeeds     int `toml:"active_seeds"`
	ActiveLimit     int `toml:"active_limit"`
	ActiveDownloads int `toml:"active_downloads"`
	FilePoolSize    int `toml:"file_pool_size"`

	UploadRateLimit int `toml:"upload_rate_limit"`

	// Sub-sections (race only)
	CustomChoking *RaceChokingConfig `toml:"custom_choking,omitempty"`

	// Advanced hoard-only: per-disk seed-slot regulation (HDD quiet mode).
	DiskSlots *DiskSlotsConfig `toml:"disk_slots,omitempty"`
}

// ArrCleanupConfig — Radarr/Sonarr cleanup settings.
type ArrCleanupConfig struct {
	RadarrURL    string  `toml:"radarr_url"`
	RadarrAPIKey string  `toml:"radarr_api_key"`
	SonarrURL    string  `toml:"sonarr_url"`
	SonarrAPIKey string  `toml:"sonarr_api_key"`
	MinScore     float64 `toml:"min_score"`
}

// VpnSpeedtestConfig — VPN iperf3 benchmark settings.
type VpnSpeedtestConfig struct {
	Enabled      bool   `toml:"enabled"`
	Iperf3Server string `toml:"iperf3_server"`
	Iperf3Port   int    `toml:"iperf3_port"`
	IntervalSecs int    `toml:"interval_secs"`
	DurationSecs int    `toml:"duration_secs"`
}

// ProxyConfig — shared SOCKS5 exit used by the Go orchestrator (getPublicIP,
// vpn_speedtest) so observed IP + measured bandwidth match the path torrents
// actually take.
type ProxyConfig struct {
	Socks5Host string `toml:"socks5_host"`
	Socks5Port int    `toml:"socks5_port"`
	Socks5User string `toml:"socks5_user"`
	Socks5Pass string `toml:"socks5_pass"`
}

// RaceDrainConfig — NVMe auto-purge settings.
type RaceDrainConfig struct {
	Enabled              bool   `toml:"enabled"`
	CheckIntervalSeconds int    `toml:"check_interval_seconds"`
	HighWatermarkPct     int    `toml:"high_watermark_pct"`
	LowWatermarkPct      int    `toml:"low_watermark_pct"`
	RacePath             string `toml:"race_path"`
	MinAgeMinutes        int    `toml:"min_age_minutes"`
	// Age/ratio auto-eviction (second trigger, independent of disk pressure).
	// Both 0 = disabled. AgeRatioMode is "and" (default) or "or".
	MaxAgeHours  int     `toml:"max_age_hours"`
	MinRatio     float64 `toml:"min_ratio"`
	AgeRatioMode string  `toml:"age_ratio_mode"`
	// Explicit on/off for the age/ratio policy (independent of the thresholds).
	AgeRatioEnabled bool `toml:"age_ratio_enabled"`
	// Action when the age/ratio trigger fires: "delete" (default) or "hoard" (graduate).
	AgeRatioAction string `toml:"age_ratio_action"`
	// API admission guard: reject race adds when the NVMe is (near) full.
	AddBlockEnabled bool `toml:"add_block_enabled"`
	ReserveFreeGB   int  `toml:"reserve_free_gb"`
}

// NotifyConfig — Discord webhook notification settings.
type NotifyConfig struct {
	Enabled    bool   `toml:"enabled"`
	WebhookURL string `toml:"webhook_url"`
}

// AuthConfig — WebUI login. password_hash is a bcrypt hash (generate with
// `hydra hash-password <pw>` or via the settings UI). Empty hash = login
// disabled (bootstrap: set a hash first). On success /api/login returns the
// API key, which the browser then uses for X-Api-Key.
type AuthConfig struct {
	Username     string `toml:"username"`
	PasswordHash string `toml:"password_hash"`
}

// DaemonConfig — top-level daemon settings (maps to [daemon] in TOML).
type DaemonConfig struct {
	APIHost             string `toml:"api_host"`
	APIPort             int    `toml:"api_port"`
	APIKey              string `toml:"api_key"`
	DataDir             string `toml:"data_dir"`
	CreateTorrentFolder bool   `toml:"create_torrent_folder"`
	UpdateCheckDisabled bool   `toml:"update_check_disabled"`
}

// HydraConfig — root configuration.
// AgentConfig describes a remote HydraAgent the front dials for multi-home
// placement. The built-in "local" agent (this process's own engines) needs no
// entry. TLSCa empty = plaintext transport.
type AgentConfig struct {
	Name  string `toml:"name"`
	Addr  string `toml:"addr"`
	Token string `toml:"token"`
	TLSCa string `toml:"tls_ca"`
}

// DiskEntry is one regulated disk in [hoard.disk_slots]; semantics live in the
// disk-slots manager (internal/engine).
type DiskEntry struct {
	Key       string `toml:"key"`
	Type      string `toml:"type"`
	MaxActive int    `toml:"max_active"`
	WakeBelow int    `toml:"wake_below"`
}

// DiskSlotsConfig is the advanced [hoard.disk_slots] section (HDD quiet mode).
// Enabled defaults false -> inert unless explicitly turned on.
type DiskSlotsConfig struct {
	Enabled              bool        `toml:"enabled"`
	Disks                []DiskEntry `toml:"disk"`
	DefaultMaxActive     int         `toml:"default_max_active"`
	SuperSeedThreshold   int         `toml:"super_seed_threshold"`
	CycleSeconds         int         `toml:"cycle_seconds"`
	PauseCooldownSec     int         `toml:"pause_cooldown_sec"`
	WakeCooldownSec      int         `toml:"wake_cooldown_sec"`
	WarmupSec            int         `toml:"warmup_sec"`
	WakeAgingBonusPerMin float64     `toml:"wake_aging_bonus_per_min"`
}

type HydraConfig struct {
	Daemon       DaemonConfig       `toml:"daemon"`
	Race         SessionConfig      `toml:"race"`
	Hoard        SessionConfig      `toml:"hoard"`
	Agents       []AgentConfig      `toml:"agent"`
	Engines      []EngineConfig     `toml:"engine"`
	ArrCleanup   ArrCleanupConfig   `toml:"arr_cleanup"`
	VpnSpeedtest VpnSpeedtestConfig `toml:"vpn_speedtest"`
	RaceDrain    RaceDrainConfig    `toml:"race_drain"`
	Notify       NotifyConfig       `toml:"notify"`
	Proxy        ProxyConfig        `toml:"proxy"`
	Auth         AuthConfig         `toml:"auth"`
	// AnnouncePasskeys: override passkey d annonce PAR-TRACKER
	// (sous-chaine host -> passkey). Annonce sous un autre compte sans
	// re-fetch des .torrent. Hot-swap via POST /api/announce/passkeys.
	AnnouncePasskeys map[string]string `toml:"announce_passkeys"`
	// AnnounceClients: spoof du client BT (peer_id + UA) PAR-TRACKER pour
	// passer les whitelists de clients (ex MAM). POST /api/announce/clients.
	AnnounceClients map[string]ClientSpoofConfig `toml:"announce_clients"`
	// AnnounceSecondaryStats: mode des stats du secondary announce PAR-TRACKER
	// (sous-chaine host -> "zero"|"off"|"clone"). "zero" evite le double-comptage
	// du downloaded sur les trackers qui somment par peer_id (seedpool, torr9).
	// Defaut (absent) = "clone". POST /api/announce/secondary-stats.
	AnnounceSecondaryStats map[string]string `toml:"announce_secondary_stats"`

	// SourcePath is the file this config was loaded from (set by Load, never
	// serialized). Used so the settings editor edits the actual --config file
	// even when it lives outside data_dir (bare-metal: /etc/hydra vs /var/lib).
	SourcePath string `toml:"-"`
}

// ClientSpoofConfig is a per-tracker fake client identity presented to a
// tracker to pass its client whitelist. Mirrors engine.ClientSpoof.
type ClientSpoofConfig struct {
	PeerIDPrefix string `toml:"peer_id_prefix"`
	UserAgent    string `toml:"user_agent"`
}

// DefaultConfig returns a config with sane defaults matching the Python version.
func DefaultConfig() *HydraConfig {
	return &HydraConfig{
		Daemon: DaemonConfig{
			APIHost:             "0.0.0.0",
			APIPort:             8199,
			APIKey:              "", // vide -> genere aleatoirement au 1er boot
			DataDir:             "/config",
			CreateTorrentFolder: false,
		},
		Race: SessionConfig{
			ListenPort:           16171,
			MaxConnections:       4000,
			MaxUploadsPerTorrent: 100,
			PeerTimeout:          30,
			InactivityTimeout:    20,
			ActiveSeeds:          50,
			ActiveLimit:          100,
			ActiveDownloads:      20,
			FilePoolSize:         500,
			CustomChoking: &RaceChokingConfig{
				Enabled:             true,
				TickIntervalSeconds: 2.0,
				Strategy:            "rarity_captive",
				MaxUnchoked:         30,
				RarityWeight:        0.7,
				SpeedWeight:         0.3,
			},
		},
		Hoard: SessionConfig{
			ListenPort:           16172,
			MaxConnections:       8000,
			MaxUploadsPerTorrent: 20,
			PeerTimeout:          90,
			InactivityTimeout:    90,
			ActiveSeeds:          -1,
			ActiveLimit:          -1,
			ActiveDownloads:      -1,
			FilePoolSize:         5000,
		},
		ArrCleanup: ArrCleanupConfig{
			MinScore: 0.6,
		},
		Auth: AuthConfig{
			Username: "admin",
		},
		RaceDrain: RaceDrainConfig{
			Enabled:              true,
			CheckIntervalSeconds: 60,
			HighWatermarkPct:     95,
			LowWatermarkPct:      85,
			RacePath:             "/race",
			MinAgeMinutes:        10,
			MaxAgeHours:          0,
			MinRatio:             0,
			AgeRatioMode:         "and",
			AgeRatioEnabled:      false,
			AgeRatioAction:       "delete",
			AddBlockEnabled:      true,
			ReserveFreeGB:        0,
		},
	}
}

// Load reads a TOML config file and returns a HydraConfig.
// Unknown keys are silently ignored (same behavior as the Python version).
// migrationKeys lists config keys introduced after v1. On load we make sure
// each exists in the file (additive only — existing lines are never touched
// or reordered), so upgrading users gain the new options and see them in the
// Configuration editor without hand-editing default.toml. Curated on purpose:
// we resurrect only keys we actually want, never the features dropped in the
// OSS cleanup. Append future keys here.
var migrationKeys = []struct{ section, key, value string }{
	{"race", "bind_interface", `""`},
	{"race", "listen_interfaces", `""`},
	{"hoard", "bind_interface", `""`},
	{"hoard", "listen_interfaces", `""`},
	{"race_drain", "max_age_hours", `0`},
	{"race_drain", "min_ratio", `0.0`},
	{"race_drain", "age_ratio_mode", `"and"`},
	{"race_drain", "age_ratio_enabled", `false`},
	{"race_drain", "age_ratio_action", `"delete"`},
	{"race_drain", "add_block_enabled", `true`},
	{"race_drain", "reserve_free_gb", `0`},
}

// ensureConfigKeys appends any missing migrationKeys to the TOML file in place.
// Best-effort: any read/parse/write error leaves the file untouched.
func ensureConfigKeys(path string) {
	data, err := os.ReadFile(path)
	if err != nil {
		return
	}
	var m map[string]interface{}
	if err := toml.Unmarshal(data, &m); err != nil {
		return
	}
	has := func(section, key string) bool {
		sec, ok := m[section].(map[string]interface{})
		if !ok {
			return false
		}
		_, ok = sec[key]
		return ok
	}
	lines := strings.Split(string(data), "\n")
	changed := false
	for _, mk := range migrationKeys {
		if has(mk.section, mk.key) {
			continue
		}
		newline := mk.key + " = " + mk.value
		hdr := "[" + mk.section + "]"
		inserted := false
		for i, ln := range lines {
			if strings.TrimSpace(ln) == hdr {
				out := make([]string, 0, len(lines)+1)
				out = append(out, lines[:i+1]...)
				out = append(out, newline)
				out = append(out, lines[i+1:]...)
				lines = out
				inserted = true
				break
			}
		}
		if !inserted {
			lines = append(lines, hdr, newline)
		}
		sec, ok := m[mk.section].(map[string]interface{})
		if !ok {
			sec = map[string]interface{}{}
			m[mk.section] = sec
		}
		sec[mk.key] = mk.value
		changed = true
	}
	if changed {
		_ = os.WriteFile(path, []byte(strings.Join(lines, "\n")), 0644)
	}
}

func Load(path string) (*HydraConfig, error) {
	ensureConfigKeys(path)
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}

	cfg := DefaultConfig()
	if err := toml.Unmarshal(data, cfg); err != nil {
		return nil, err
	}
	cfg.SourcePath = path

	return cfg, nil
}
