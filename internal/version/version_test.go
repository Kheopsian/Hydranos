package version

import "testing"

// The invariant that matters: whatever Version currently says has to encode,
// and it has to encode to exactly 8 bytes. A 21-byte peer_id is rejected by
// trackers with "invalid peer_id length", which is how this last broke.
func TestVersionEncodesToEightBytes(t *testing.T) {
	if got := PeerFingerprint(); len(got) != 8 {
		t.Fatalf("PeerFingerprint() = %q, len %d, want 8", got, len(got))
	}
}

func TestPeerFingerprintEncoding(t *testing.T) {
	cases := []struct{ in, want string }{
		{"0.0.0", "-HY0000-"},
		{"3.0.0", "-HY3000-"},
		{"3.9.9", "-HY3099-"},
		{"3.61.0", "-HY30z0-"},        // the last minor one character holds
		{"3.62.0", "-HY3100-"},        // the first that needs two
		{"3.97.0-typhon", "-HY31Z0-"}, // the suffix is not part of the version
		{"3.98.0-typhon", "-HY31a0-"},
		{"3.100.0", "-HY31c0-"}, // where the decimal encoding used to overflow
		{"3.1295.0", "-HY3Kt0-"},
		{"61.3843.61", "-HYzzzz-"}, // every field at capacity
	}

	for _, tc := range cases {
		got := mustPeerFingerprint(tc.in)
		if got != tc.want {
			t.Errorf("mustPeerFingerprint(%q) = %q, want %q", tc.in, got, tc.want)
		}
		if len(got) != 8 {
			t.Errorf("mustPeerFingerprint(%q) = %q, len %d, want 8", tc.in, got, len(got))
		}
	}
}

// A version that does not fit has to stop the process, not be truncated into
// something that looks plausible on the wire.
func TestPeerFingerprintRefusesWhatItCannotEncode(t *testing.T) {
	for _, v := range []string{
		"62.0.0",    // major past capacity
		"3.3844.0",  // minor past capacity
		"3.0.62",    // patch past capacity
		"3.0",       // not three fields
		"3.0.0.0",   // four fields
		"",          // empty
		"3.x.0",     // not a number
		"three.0.0", // not a number
		"3..0",      // empty field
	} {
		func() {
			defer func() {
				if recover() == nil {
					t.Errorf("mustPeerFingerprint(%q) returned instead of panicking", v)
				}
			}()
			_ = mustPeerFingerprint(v)
		}()
	}
}

func TestParseVersionIgnoresSuffix(t *testing.T) {
	major, minor, patch, ok := parseVersion("3.98.0-typhon")
	if !ok || major != 3 || minor != 98 || patch != 0 {
		t.Fatalf("parseVersion = %d.%d.%d ok=%v, want 3.98.0 ok=true", major, minor, patch, ok)
	}
}
