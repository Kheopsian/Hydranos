package engine

import "testing"

// The override maps used to be matched with strings.Contains over the whole
// tracker URL, walking a Go map. Two problems, both proven below: a short key
// matched a longer unrelated host ("torr" vs "torr9.net"), and with several
// matching keys the winner depended on the randomised map order.

func TestPasskeyOverrideDoesNotMatchUnrelatedHost(t *testing.T) {
	passkeyOverrideMu.Lock()
	passkeyOverrides = map[string]string{"torr": "SECRET-TORR"}
	passkeyOverrideMu.Unlock()
	defer func() {
		passkeyOverrideMu.Lock()
		passkeyOverrides = map[string]string{}
		passkeyOverrideMu.Unlock()
	}()

	// torr9.net is a different tracker. Sending it torr's passkey leaks one
	// tracker's credential to another.
	const u = "https://torr9.net/announce/MYOWNKEY"
	if got := applyPasskeyOverride(u); got != u {
		t.Fatalf("passkey of host %q leaked to unrelated host: got %q, want %q", "torr", got, u)
	}

	// The exact host still gets its override.
	const v = "https://torr/announce/PLACEHOLDER"
	if got, want := applyPasskeyOverride(v), "https://torr/announce/SECRET-TORR"; got != want {
		t.Fatalf("exact host not overridden: got %q, want %q", got, want)
	}

	// A subdomain matches on the dot boundary.
	passkeyOverrideMu.Lock()
	passkeyOverrides = map[string]string{"tr4ker.net": "PK"}
	passkeyOverrideMu.Unlock()
	const w = "https://tk.tr4ker.net/announce/PLACEHOLDER"
	if got, want := applyPasskeyOverride(w), "https://tk.tr4ker.net/announce/PK"; got != want {
		t.Fatalf("dot-boundary suffix not matched: got %q, want %q", got, want)
	}
}

func TestOverlappingKeysResolveToLongestDeterministically(t *testing.T) {
	passkeyOverrideMu.Lock()
	passkeyOverrides = map[string]string{"tr4ker.net": "BROAD", "tk.tr4ker.net": "SPECIFIC"}
	passkeyOverrideMu.Unlock()
	defer func() {
		passkeyOverrideMu.Lock()
		passkeyOverrides = map[string]string{}
		passkeyOverrideMu.Unlock()
	}()

	const u = "https://tk.tr4ker.net/announce/PLACEHOLDER"
	const want = "https://tk.tr4ker.net/announce/SPECIFIC"
	// Map order is randomised per range: one pass can pick the right key by
	// luck, so repeat enough that a nondeterministic implementation fails.
	for i := 0; i < 200; i++ {
		if got := applyPasskeyOverride(u); got != want {
			t.Fatalf("iteration %d: got %q, want %q (longest key must win)", i, got, want)
		}
	}
}

func TestClientOverrideMatchesOnHostOnly(t *testing.T) {
	clientOverrideMu.Lock()
	clientOverrides = map[string]ClientSpoof{"torr": {PeerIDPrefix: "-XX0000-"}}
	clientOverrideMu.Unlock()
	defer func() {
		clientOverrideMu.Lock()
		clientOverrides = map[string]ClientSpoof{}
		clientOverrideMu.Unlock()
	}()

	if _, ok := clientOverrideFor("https://torr9.net/announce"); ok {
		t.Fatal("client spoof of host \"torr\" applied to unrelated host torr9.net")
	}
	// The key must not match merely by appearing in the path or query either.
	if _, ok := clientOverrideFor("https://example.org/announce?src=torr"); ok {
		t.Fatal("client spoof matched on the query string, not the host")
	}
	if _, ok := clientOverrideFor("https://torr/announce"); !ok {
		t.Fatal("exact host lost its override")
	}
}

func TestAnnounceIPModeAcceptsURLHostAndDialTarget(t *testing.T) {
	announceIPModeMu.Lock()
	announceIPModes = map[string]string{"tr4ker.net": AnnounceIPModeV4}
	announceIPModeMu.Unlock()
	defer func() {
		announceIPModeMu.Lock()
		announceIPModes = map[string]string{}
		announceIPModeMu.Unlock()
	}()

	// announceIPModeFor is called with all three shapes; the dial path passes
	// "host:port", so a URL-only parser would silently stop pinning the family.
	for _, in := range []string{
		"https://tk.tr4ker.net/announce/abc",
		"tk.tr4ker.net",
		"tk.tr4ker.net:443",
	} {
		if got := announceIPModeFor(in); got != AnnounceIPModeV4 {
			t.Errorf("announceIPModeFor(%q) = %q, want %q", in, got, AnnounceIPModeV4)
		}
	}
	if got := announceIPModeFor("other.invalid:443"); got != AnnounceIPModeAuto {
		t.Errorf("unrelated host pinned: got %q, want auto", got)
	}
}
