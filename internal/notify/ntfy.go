package notify

import (
	"bytes"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"time"
)

// Ntfy pushes a short notification to a ntfy.sh topic (the operator's phone).
// Edge-triggered alerting only — never a heartbeat.
type Ntfy struct {
	url    string
	client *http.Client
}

// NewNtfy builds a sender for the topic, or returns nil if topic is empty
// (nil is a safe no-op receiver, so callers need no guard).
func NewNtfy(topic string) *Ntfy {
	if topic == "" {
		return nil
	}
	return &Ntfy{
		url:    "https://ntfy.sh/" + topic,
		client: &http.Client{Timeout: 10 * time.Second},
	}
}

// Send posts a message. priority is one of min/low/default/high/urgent; tags is
// a comma-separated ntfy tag list. Both optional. No-op on a nil receiver.
func (n *Ntfy) Send(title, message, priority, tags string) {
	if n == nil {
		return
	}
	req, err := http.NewRequest(http.MethodPost, n.url, bytes.NewReader([]byte(message)))
	if err != nil {
		return
	}
	if title != "" {
		req.Header.Set("Title", title)
	}
	if priority != "" {
		req.Header.Set("Priority", priority)
	}
	if tags != "" {
		req.Header.Set("Tags", tags)
	}
	resp, err := n.client.Do(req)
	if err != nil {
		slog.Warn("ntfy: send failed", "error", err)
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 300 {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 512))
		slog.Warn("ntfy: non-2xx response", "status", resp.StatusCode, "body", strings.TrimSpace(string(body)))
	}
}
