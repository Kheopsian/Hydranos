package config

import (
	"os"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

// RaceChokingConfig — custom choking algorithm settings.
//
// The json tags here and on every struct a front pushes to an agent
// (SessionConfig, DiskSlotsConfig, DiskEntry, ClientSpoofConfig) mirror the
// toml ones on purpose: the agent wire carries these types verbatim rather
// than re-declaring them (see internal/agentwire), so an operator reads the
// same key names in default.toml, in an apply_config frame and in the agent's
// cached pushed-config.json.
type RaceChokingConfig struct {
	Enabled             bool    `toml:"enabled" json:"enabled"`
	TickIntervalSeconds float64 `toml:"tick_interval_seconds" json:"tick_interval_seconds"`
	Strategy            string  `toml:"strategy" json:"strategy"`
	MaxUnchoked         int     `toml:"max_unchoked" json:"max_unchoked"`
	RarityWeight        float64 `toml:"rarity_weight" json:"rarity_weight"`
	SpeedWeight         float64 `toml:"speed_weight" json:"speed_weight"`
}

// SessionConfig — engine session parameters (race or hoard). Only fields the
// Typhon engine actually consumes are kept; libtorrent-era tuning knobs were
// removed (see git history — they were packed into the engine JSON but ignored).
type SessionConfig struct {
	ListenPort       int    `toml:"listen_port" json:"listen_port"`
	ListenInterfaces string `toml:"listen_interfaces" json:"listen_interfaces"`
	BindInterface    string `toml:"bind_interface" json:"bind_interface"` // interface NAME (e.g. "wg0"); resolved to its IP at engine start
	// Listen for peers over IPv6 too, and take the v6 peers trackers and PEX
	// offer. Off by default: v4 only, which is what every install has run so
	// far. Enabling it adds a v6-only listener beside the v4 one rather than
	// replacing it, so v4 peers keep their v4 addresses everywhere.
	EnableIPv6 bool `toml:"enable_ipv6" json:"enable_ipv6"`
	// Peer discovery that does not go through a tracker. Both default to true
	// (DefaultConfig), and Reload decodes the file over those defaults, so a
	// config written before these keys existed keeps the behaviour it had.
	// Both already skip `private` torrents; turning them off is for the
	// operator who wants this engine to reach nothing but its trackers.
	EnableDHT bool `toml:"enable_dht" json:"enable_dht"`
	EnablePEX bool `toml:"enable_pex" json:"enable_pex"`
	// EnableWebseed turns on BEP 19: when a torrent names HTTP mirrors in
	// its url-list, fetch from them. Torrents published without a seeder
	// (every Internet Archive item) need this to move at all.
	EnableWebseed bool `toml:"enable_webseed" json:"enable_webseed"`
	// WebseedMaxConcurrent caps webseed fetches in flight per engine.
	WebseedMaxConcurrent int `toml:"webseed_max_concurrent" json:"webseed_max_concurrent"`
	// Optional second TCP listener expecting HAProxy PROXY protocol v2 (real
	// peer IP in header). Used by the v6 bypass path: peer → VPS haproxy →
	// the router rdr → the seedbox host [::]:listen_port_proxy_v2. 0 = disabled.
	ListenPortProxyV2 int `toml:"listen_port_proxy_v2" json:"listen_port_proxy_v2"`
	// Explicit bind addr for the PROXY v2 listener (e.g. "[2001:db8:abc:d12::2]").
	// Empty = [::] wildcard (risk of source-selection bug with multiple v6 IPs).
	ListenAddrProxyV2     string   `toml:"listen_addr_proxy_v2" json:"listen_addr_proxy_v2"`
	ProxyV2TrustedSources []string `toml:"proxy_v2_trusted_sources" json:"proxy_v2_trusted_sources"`
	Socks5OutboundHost    string   `toml:"socks5_outbound_host" json:"socks5_outbound_host"`
	Socks5OutboundPort    int      `toml:"socks5_outbound_port" json:"socks5_outbound_port"`
	Socks5OutboundUser    string   `toml:"socks5_outbound_user" json:"socks5_outbound_user"`
	Socks5OutboundPass    string   `toml:"socks5_outbound_pass" json:"socks5_outbound_pass"`
	// AnnounceProxy routes this session's tracker announces through a SOCKS5
	// proxy ("socks5h://user:pass@host:port"). Empty falls back to the legacy
	// TYPHON_ANNOUNCE_PROXY env, and failing that the announce goes direct.
	//
	// Deliberately separate from socks5_outbound_* above, which only covers
	// PEER dials inside the engine: announces are issued by the Go side and
	// never consulted that setting. A relay setup that set only the
	// socks5_outbound_* keys therefore kept announcing from the host's own
	// address, and the tracker recorded it — the leak this key closes.
	// Note that UDP trackers are skipped while it is set: SOCKS5 carries TCP
	// only, and falling back to a direct datagram would leak the address the
	// operator asked us to hide.
	AnnounceProxy string `toml:"announce_proxy" json:"announce_proxy"`
	// AnnounceIP is the address advertised to trackers in the BEP-7 `ip=`
	// parameter. Empty (the default) omits the parameter entirely and lets the
	// tracker observe the announce's source address, which is correct whenever
	// the announce already leaves by the path peers should reach us on. Set it
	// when the two differ and the tracker honours the parameter — many do not.
	AnnounceIP string `toml:"announce_ip" json:"announce_ip"`
	// GluetunPortForward makes this session take its listen port from gluetun
	// rather than from listen_port. A VPN provider assigns the forwarded port
	// per lease and rotates it, so a fixed port behind such a tunnel is wrong
	// the moment the lease turns over, silently: the node keeps announcing a
	// port that answers nobody.
	//
	// While it is on, announces and peer dials are HELD at startup until the
	// port is known. Announcing first would publish the wrong port to every
	// tracker, and they keep it for a whole announce cycle.
	GluetunPortForward bool `toml:"gluetun_port_forward" json:"gluetun_port_forward"`
	// GluetunURL is gluetun's control server. Empty = http://127.0.0.1:8000,
	// which is where it listens when Hydra shares gluetun's network namespace.
	GluetunURL string `toml:"gluetun_url" json:"gluetun_url"`
	// GluetunAPIKey authenticates against that control server. Recent gluetun
	// versions refuse every request without one; the role needs the route
	// GET /v1/portforward.
	GluetunAPIKey string `toml:"gluetun_api_key" json:"gluetun_api_key"`
	// WireGuard makes this engine bring its own tunnel up rather than expecting
	// one to already exist. When it is on, bind_interface is DERIVED from it
	// and any hand-written value is overridden: two writers for one decision is
	// how an engine ends up pinned to a tunnel it does not own.
	WireGuard            *WireGuardConfig `toml:"wireguard,omitempty" json:"wireguard,omitempty"`
	MaxConnections       int              `toml:"max_connections" json:"max_connections"`
	MaxUploadsPerTorrent int              `toml:"max_uploads_per_torrent" json:"max_uploads_per_torrent"`

	// AnnounceRateLimit caps outbound tracker announces for this session, in
	// announces per second. 0 = unlimited (the historical behaviour). A large
	// hoard announces in waves; behind a VPN that wave is a burst of new flows
	// through one tunnel, and some providers (Proton) throttle or drop its
	// tail, which surfaces as tracker failures. Smoothing the same volume over
	// time fixes that. Fractional values are allowed (0.5 = one announce every
	// two seconds).
	AnnounceRateLimit float64 `toml:"announce_rate_limit" json:"announce_rate_limit"`

	// MaxDialsPerSec caps new outbound peer connections for this session, in
	// dials per second. 0 = unlimited (the historical behaviour).
	//
	// AnnounceRateLimit alone is not enough: an announce asks for numwant=200
	// peers and the engine dials every one it gets back, so 20 announces/s can
	// still mean thousands of new flows a second through one VPN tunnel. This
	// is the equivalent of qBittorrent's connections-per-second knob, and it
	// is the one that bounds what a tunnel actually sees.
	MaxDialsPerSec float64 `toml:"max_dials_per_sec" json:"max_dials_per_sec"`

	// StartPaused holds this session's outbound traffic at startup: no
	// announces leave and no peers are dialed until the pause is released
	// from the UI or the API. Intended for large libraries behind a VPN,
	// where the boot-time wave is the thing that knocks the tunnel over and
	// the user needs a moment to adjust limits before it happens.
	//
	// This is a process-level hold, never per-torrent paused state: releasing
	// it must not resume torrents the user paused deliberately.
	StartPaused bool `toml:"start_paused" json:"start_paused"`

	PeerTimeout       int `toml:"peer_timeout" json:"peer_timeout"`
	InactivityTimeout int `toml:"inactivity_timeout" json:"inactivity_timeout"`

	ActiveSeeds     int `toml:"active_seeds" json:"active_seeds"`
	ActiveLimit     int `toml:"active_limit" json:"active_limit"`
	ActiveDownloads int `toml:"active_downloads" json:"active_downloads"`
	FilePoolSize    int `toml:"file_pool_size" json:"file_pool_size"`

	UploadRateLimit int `toml:"upload_rate_limit" json:"upload_rate_limit"`

	// Sub-sections (race only)
	CustomChoking *RaceChokingConfig `toml:"custom_choking,omitempty" json:"custom_choking,omitempty"`

	// Advanced hoard-only: per-disk seed-slot regulation (HDD quiet mode).
	DiskSlots *DiskSlotsConfig `toml:"disk_slots,omitempty" json:"disk_slots,omitempty"`
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

// BenchConfig — bounds on the bench/telemetry database (bench.db).
//
// race_snapshots records one row per racing torrent every sampling interval.
// It shares a single year-long retention with the low-rate tables, which lets
// it grow to multi-GB on a large instance before a single row is pruned, so it
// gets its own window here.
type BenchConfig struct {
	// Enabled=false stops every bench writer (samples, race snapshots, race
	// events, tracker samples). Reads keep working on what is already stored.
	Enabled *bool `toml:"enabled"`
	// RetentionDays bounds the low-rate tables. 0 = 365. Negative = keep all.
	RetentionDays int `toml:"retention_days"`
	// RaceSnapshotRetentionDays bounds race_snapshots. 0 = 7. Negative = keep all.
	RaceSnapshotRetentionDays int `toml:"race_snapshot_retention_days"`
	// SnapshotIntervalSecs is the race sampling cadence. 0 = 5.
	SnapshotIntervalSecs int `toml:"snapshot_interval_secs"`
	// PruneIntervalMins is how often retention runs. 0 = 60.
	PruneIntervalMins int `toml:"prune_interval_mins"`
	// Vacuum reclaims the file after a prune that deleted rows. Off by
	// default: VACUUM rewrites the whole DB and needs as much free space again.
	Vacuum bool `toml:"vacuum"`
}

// BenchEnabled reports whether bench recording is on (absent key = on, which
// is what every install ran before this section existed).
func (b BenchConfig) BenchEnabled() bool {
	return b.Enabled == nil || *b.Enabled
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
	APIHost string `toml:"api_host"`
	APIPort int    `toml:"api_port"`
	APIKey  string `toml:"api_key"`
	// AgentToken is the shared bearer token the HydraAgent gRPC data-plane
	// requires from a front (--agent-addr / --agent-only). It lives here so an
	// agent node can be handed its token by the config file it already mounts
	// instead of a command line: --agent-token and $HYDRA_AGENT_TOKEN both
	// override it. Empty everywhere = the data-plane serves without auth.
	AgentToken          string `toml:"agent_token"`
	DataDir             string `toml:"data_dir"`
	CreateTorrentFolder bool   `toml:"create_torrent_folder"`
	UpdateCheckDisabled bool   `toml:"update_check_disabled"`
	// MoveMaxMBPerSec caps how fast a payload move copies, in MB/s.
	//
	// A move that crosses filesystems reads and writes the same disks the
	// torrents are served from, sequentially and greedily; left uncapped it
	// takes whatever the array can give and the seeding feels it. Only the
	// copy path is affected -- a same-filesystem move is a rename and moves
	// no bytes at all.
	//
	// A pointer so that "not configured" and "configured to zero" are
	// different answers: unset gets DefaultMoveMaxMBPerSec, an explicit 0
	// means no cap, and anything else is taken literally.
	MoveMaxMBPerSec *int `toml:"move_max_mb_per_sec"`
}

// DefaultMoveMaxMBPerSec is the throughput cap applied when the config does
// not set one.
//
// 200 MB/s sits clearly below what the array sustains while still finishing a
// large release in a sensible time: a 100 GB move takes about eight minutes, a
// 400 GB one a little over half an hour. Production already serves a few
// hundred MB/s of seeding off these same disks, so an uncapped copy competing
// for them is noticed immediately, whereas this leaves the bulk of the
// throughput to the thing users actually see. It is one line to change, and
// the honest way to tune it is to watch the first real move.
const DefaultMoveMaxMBPerSec = 200

// MoveBytesPerSecond converts the configured cap into the byte rate the mover
// wants: zero means uncapped, unset means the default.
func (d DaemonConfig) MoveBytesPerSecond() int64 {
	mb := DefaultMoveMaxMBPerSec
	if d.MoveMaxMBPerSec != nil {
		mb = *d.MoveMaxMBPerSec
	}
	if mb <= 0 {
		return 0
	}
	return int64(mb) * 1024 * 1024
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
	// Role, EngineID and Session describe an agent that runs HERE: no addr, so
	// nothing is dialled and this process starts the engine itself. They are
	// the shape the config is converging on -- one [[agent]] array for every
	// node, local or remote -- and they are ignored on an entry that has an
	// addr, where the engine's settings live on the far side.
	Role     string `toml:"role"`
	EngineID string `toml:"engine_id"`
	// Session is a SPARSE override of the role profile, not a whole config.
	// Kept as a raw map for the same reason EngineOverrides is: decoded into a
	// SessionConfig, "absent" and "written as zero" become the same thing, and
	// an entry carrying three keys would run with max_connections = 0 and
	// peer_timeout = 0 -- a config nobody wrote and nothing reports.
	Session map[string]interface{} `toml:"session"`
	// EngineOverrides are sparse per-engine exceptions to the [race]/[hoard]
	// fleet profile, written as [[agent.engine]] blocks keyed by engine id.
	// Kept as raw maps rather than a typed SessionConfig so "absent" and "set
	// to the zero value" stay distinguishable without turning every field of
	// SessionConfig into a pointer; ComposeSession merges them key by key.
	EngineOverrides []map[string]interface{} `toml:"engine"`
}

// DiskEntry is one regulated disk in [hoard.disk_slots]; semantics live in the
// disk-slots manager (internal/engine).
type DiskEntry struct {
	Key       string `toml:"key" json:"key"`
	Type      string `toml:"type" json:"type"`
	MaxActive int    `toml:"max_active" json:"max_active"`
	WakeBelow int    `toml:"wake_below" json:"wake_below"`
}

// DiskSlotsConfig is the advanced [hoard.disk_slots] section (HDD quiet mode).
// Enabled defaults false -> inert unless explicitly turned on.
type DiskSlotsConfig struct {
	Enabled              bool        `toml:"enabled" json:"enabled"`
	Disks                []DiskEntry `toml:"disk" json:"disk"`
	DefaultMaxActive     int         `toml:"default_max_active" json:"default_max_active"`
	SuperSeedThreshold   int         `toml:"super_seed_threshold" json:"super_seed_threshold"`
	CycleSeconds         int         `toml:"cycle_seconds" json:"cycle_seconds"`
	PauseCooldownSec     int         `toml:"pause_cooldown_sec" json:"pause_cooldown_sec"`
	WakeCooldownSec      int         `toml:"wake_cooldown_sec" json:"wake_cooldown_sec"`
	WarmupSec            int         `toml:"warmup_sec" json:"warmup_sec"`
	WakeAgingBonusPerMin float64     `toml:"wake_aging_bonus_per_min" json:"wake_aging_bonus_per_min"`
}

type HydraConfig struct {
	Daemon       DaemonConfig       `toml:"daemon"`
	Race         SessionConfig      `toml:"race"`
	Hoard        SessionConfig      `toml:"hoard"`
	Agents       []AgentConfig      `toml:"agent"`
	Engines      []EngineConfig     `toml:"engine"`
	ArrCleanup   ArrCleanupConfig   `toml:"arr_cleanup"`
	VpnSpeedtest VpnSpeedtestConfig `toml:"vpn_speedtest"`
	Bench        BenchConfig        `toml:"bench"`
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
	// AnnounceIPModes: famille d adresse des annonces PAR-TRACKER
	// (sous-chaine host -> "v4"|"v6"|"auto"). Defaut (absent) = "auto" = une
	// annonce par famille disponible, meme peer_id, comme libtorrent : le
	// tracker garde UN pair joignable aux deux adresses. Forcer "v4"/"v6"
	// seulement pour un tracker qui compte les deux adresses comme deux pairs
	// ou qui plafonne les pairs par compte. POST /api/announce/ip-modes.
	AnnounceIPModes map[string]string `toml:"announce_ip_modes"`

	// SourcePath is the file this config was loaded from (set by Load, never
	// serialized). Used so the settings editor edits the actual --config file
	// even when it lives outside data_dir (bare-metal: /etc/hydra vs /var/lib).
	SourcePath string `toml:"-"`
}

// ClientSpoofConfig is a per-tracker fake client identity presented to a
// tracker to pass its client whitelist. Mirrors engine.ClientSpoof.
type ClientSpoofConfig struct {
	PeerIDPrefix string `toml:"peer_id_prefix" json:"peer_id_prefix"`
	UserAgent    string `toml:"user_agent" json:"user_agent"`
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
			EnableDHT:            true,
			EnablePEX:            true,
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
			EnableDHT:            true,
			EnablePEX:            true,
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
	{"daemon", "agent_token", `""`},
	{"race", "bind_interface", `""`},
	{"race", "enable_ipv6", `false`},
	{"race", "enable_dht", `true`},
	{"race", "enable_pex", `true`},
	{"race", "enable_webseed", `true`},
	{"race", "webseed_max_concurrent", `48`},
	{"race", "listen_interfaces", `""`},
	{"race", "announce_rate_limit", `0.0`},
	{"race", "max_dials_per_sec", `0.0`},
	{"race", "start_paused", `false`},
	{"hoard", "bind_interface", `""`},
	{"hoard", "enable_ipv6", `false`},
	{"hoard", "enable_dht", `true`},
	{"hoard", "enable_pex", `true`},
	{"hoard", "enable_webseed", `true`},
	{"hoard", "webseed_max_concurrent", `48`},
	{"hoard", "listen_interfaces", `""`},
	{"hoard", "announce_rate_limit", `0.0`},
	{"hoard", "max_dials_per_sec", `0.0`},
	{"hoard", "start_paused", `false`},
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
	return Reload(path)
}

// Reload re-reads a config file WITHOUT the additive migration pass. The
// migration rewrites the file, which is right once at boot and wrong for a
// caller that re-reads it on a timer: the front recomposes what it pushes to
// its agents from the file on every reconcile tick, because the settings
// editor writes there and does not update the in-memory struct.
func Reload(path string) (*HydraConfig, error) {
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
