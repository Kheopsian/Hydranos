package api

import (
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// The refusal the maintainer hit: a Tailscale address is not RFC1918, so
// net.IP.IsPrivate() says false and first-run setup told a user on their own
// tailnet that they were on the public internet.
func TestFirstRunAcceptsTailnetAddresses(t *testing.T) {
	allowed := []string{
		"127.0.0.1", "::1",
		"192.168.1.42", "10.1.2.3", "172.16.0.9",
		"fd7a:115c:a1e0::1", // RFC4193, Tailscale's IPv6 range
		"100.109.249.37",    // RFC 6598, the one that was refused
		"100.64.0.1", "100.127.255.254",
	}
	for _, ip := range allowed {
		if !isLocalRequest(ip) {
			t.Errorf("%s was refused; a user there cannot complete first-run setup at all", ip)
		}
	}
	// The guard still has to do its job: an instance exposed to the internet
	// before its owner finished setting it up must not be claimable.
	refused := []string{"8.8.8.8", "203.0.113.7", "2001:4860:4860::8888", "100.128.0.1", "99.255.255.255", "not-an-ip"}
	for _, ip := range refused {
		if isLocalRequest(ip) {
			t.Errorf("%s was accepted; a stranger could claim an unconfigured instance", ip)
		}
	}
}

// The escape hatch every lockout message points at refused to work on a config
// that had never carried an [auth] section -- which is exactly the config a
// first run has.
func TestAuthSectionIsCreatedWhenAbsent(t *testing.T) {
	doc := "[daemon]\napi_port = 8080\n"
	out, err := config.SetTOMLTable(doc, "auth", [][2]string{
		{"username", `"admin"`},
		{"password_hash", `"$2a$10$fake"`},
	})
	if err != nil {
		t.Fatalf("could not add an [auth] section: %v", err)
	}
	if !strings.Contains(out, "[auth]") || !strings.Contains(out, "password_hash") {
		t.Errorf("the [auth] section was not written:\n%s", out)
	}
	// And the rest of the file survives.
	if !strings.Contains(out, "api_port = 8080") {
		t.Errorf("the edit lost the existing config:\n%s", out)
	}
}
