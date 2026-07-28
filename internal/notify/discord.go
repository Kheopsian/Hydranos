package notify

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"sync"
	"time"
)

// Colors for Discord embeds.
const (
	ColorGreen  = 0x3fb950
	ColorOrange = 0xf0883e
	ColorRed    = 0xf85149
	ColorBlue   = 0x58a6ff
	ColorGray   = 0x7a90a8
)

// Notifier sends messages to a Discord webhook.
type Notifier struct {
	webhookURL string
	mu         sync.Mutex
	lastSent   map[string]time.Time // dedup key -> last sent time
	cooldown   time.Duration
}

// New creates a new Discord notifier. If webhookURL is empty, all sends are no-ops.
func New(webhookURL string) *Notifier {
	return &Notifier{
		webhookURL: webhookURL,
		lastSent:   make(map[string]time.Time),
		cooldown:   5 * time.Minute,
	}
}

// Enabled returns true if a webhook URL is configured.
func (n *Notifier) Enabled() bool {
	return n.webhookURL != ""
}

type discordEmbed struct {
	Title       string         `json:"title,omitempty"`
	Description string         `json:"description,omitempty"`
	Color       int            `json:"color,omitempty"`
	Fields      []discordField `json:"fields,omitempty"`
	Timestamp   string         `json:"timestamp,omitempty"`
}

type discordField struct {
	Name   string `json:"name"`
	Value  string `json:"value"`
	Inline bool   `json:"inline,omitempty"`
}

type discordPayload struct {
	Content string         `json:"content,omitempty"`
	Embeds  []discordEmbed `json:"embeds,omitempty"`
}

// Send sends an embed to Discord. dedupKey prevents spamming the same event
// within the cooldown period. Pass "" to disable dedup.
func (n *Notifier) Send(dedupKey, title, description string, color int, fields map[string]string) {
	if !n.Enabled() {
		return
	}

	// Dedup check.
	if dedupKey != "" {
		n.mu.Lock()
		if last, ok := n.lastSent[dedupKey]; ok && time.Since(last) < n.cooldown {
			n.mu.Unlock()
			return
		}
		n.lastSent[dedupKey] = time.Now()
		n.mu.Unlock()
	}

	embed := discordEmbed{
		Title:       title,
		Description: description,
		Color:       color,
		Timestamp:   time.Now().Format(time.RFC3339),
	}

	for k, v := range fields {
		embed.Fields = append(embed.Fields, discordField{Name: k, Value: v, Inline: true})
	}

	payload := discordPayload{Embeds: []discordEmbed{embed}}

	go n.post(payload)
}

func (n *Notifier) post(payload discordPayload) {
	body, err := json.Marshal(payload)
	if err != nil {
		slog.Error("notify: marshal error", "error", err)
		return
	}

	resp, err := http.Post(n.webhookURL, "application/json", bytes.NewReader(body))
	if err != nil {
		slog.Error("notify: send error", "error", err)
		return
	}
	defer resp.Body.Close()

	if resp.StatusCode == 429 {
		slog.Warn("notify: rate limited by Discord")
		return
	}
	if resp.StatusCode >= 400 {
		slog.Warn("notify: Discord returned error", "status", resp.StatusCode)
		return
	}

	slog.Debug("notify: sent", "title", payload.Embeds[0].Title)
}

// --- Convenience methods for common events ---

// RaceCompleted notifies when a race torrent finishes downloading.
func (n *Notifier) RaceCompleted(name string, ratio float64, seeds int, share float64, size int64) {
	n.Send("", "Race completed", name, ColorOrange, map[string]string{
		"Ratio": fmt.Sprintf("%.2f", ratio),
		"Seeds": fmt.Sprintf("%d", seeds),
		"Share": fmt.Sprintf("%.1f%%", share*100),
		"Size":  formatBytes(size),
	})
}

// VpnDown notifies when the VPN speedtest fails or returns 0.
func (n *Notifier) VpnDown(detail string) {
	n.Send("vpn_down", "VPN Down", detail, ColorRed, nil)
}

// VpnDegraded notifies when VPN speed drops below a threshold.
func (n *Notifier) VpnDegraded(ulMbps, dlMbps float64) {
	n.Send("vpn_degraded", "VPN Degraded", "Speed below expected", ColorOrange, map[string]string{
		"Upload":   fmt.Sprintf("%.1f Mbps", ulMbps),
		"Download": fmt.Sprintf("%.1f Mbps", dlMbps),
	})
}

// HoardVerifyDone notifies when hoard verification is complete.
func (n *Notifier) HoardVerifyDone(total, seeding, errors int) {
	n.Send("hoard_verify", "Hoard Verification Complete", "", ColorGreen, map[string]string{
		"Total":   fmt.Sprintf("%d", total),
		"Seeding": fmt.Sprintf("%d", seeding),
		"Errors":  fmt.Sprintf("%d", errors),
	})
}

// TorrentError notifies when a torrent encounters an error.
func (n *Notifier) TorrentError(name, errorMsg string) {
	n.Send("err_"+name, "Torrent Error", name, ColorRed, map[string]string{
		"Error": errorMsg,
	})
}

func formatBytes(b int64) string {
	const unit = 1024
	if b < unit {
		return fmt.Sprintf("%d B", b)
	}
	div, exp := int64(unit), 0
	for n := b / unit; n >= unit; n /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", float64(b)/float64(div), "KMGTPE"[exp])
}
