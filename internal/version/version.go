package version

import "strings"

// Version is the single source of truth.
var Version = "3.105.0-typhon"

// b62 is digits, then upper case, then lower case. It is the alphabet
// Transmission's clients.cc decodes with base62str, and a strict superset of
// the base36 libtorrent writes in version_to_char: the two agree on 0..35, so
// nothing below 36 diverges from what every other client emits.
const b62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"

// Capacities of the four characters between "-HY" and the closing "-". Two of
// them go to the minor because Hydra bumps it once per feature, roughly four
// times a day: it passed 61 -- what every other client fits in one character
// -- three weeks after 3.0.0.
const (
	MaxMajor = 61
	MaxMinor = 62*62 - 1
	MaxPatch = 61
)

// peerFingerprintID is derived from Version once, at package init, so a
// version that cannot be encoded fails at startup and under test rather than
// on the wire.
//
// It replaces a literal that was kept in step with Version by hand and encoded
// the version in decimal across four characters. That encoding overflowed into
// a ninth byte at 3.100.0, which a length guard then truncated back to eight,
// dropping the closing dash and leaving a malformed prefix that trackers still
// accepted. Deriving it removes both the second copy and the truncation.
var peerFingerprintID = mustPeerFingerprint(Version)

// PeerFingerprint returns the 8-byte Azureus-style BEP-20 peer_id prefix
// ("-HY####-"). The engine appends a 12-byte random suffix, so 8 + 12 = 20,
// the only peer_id length trackers accept.
//
// No third-party reader decodes these four characters the way they are written
// here, and none did before either: a generic Azureus-style parser reads four
// independent single-character fields, so libtorrent renders 3.97.0 as
// "HY 3.9.7.0" whatever we choose. The encoding is for us; its only hard
// requirement is the length.
func PeerFingerprint() string {
	return peerFingerprintID
}

func mustPeerFingerprint(v string) string {
	major, minor, patch, ok := parseVersion(v)
	if !ok {
		panic("version: cannot read " + v + " as major.minor.patch")
	}
	if major > MaxMajor || minor > MaxMinor || patch > MaxPatch {
		panic("version: " + v + " does not fit the 4-character peer_id fingerprint")
	}
	return "-HY" + string(b62[major]) +
		string(b62[minor/62]) + string(b62[minor%62]) +
		string(b62[patch]) + "-"
}

// parseVersion reads the leading major.minor.patch of a version string,
// ignoring any pre-release or build suffix: "3.98.0-typhon" is 3, 98, 0.
func parseVersion(v string) (major, minor, patch int, ok bool) {
	if i := strings.IndexAny(v, "-+"); i >= 0 {
		v = v[:i]
	}
	parts := strings.Split(v, ".")
	if len(parts) != 3 {
		return 0, 0, 0, false
	}
	var out [3]int
	for i, p := range parts {
		if p == "" {
			return 0, 0, 0, false
		}
		n := 0
		for _, c := range p {
			if c < '0' || c > '9' {
				return 0, 0, 0, false
			}
			n = n*10 + int(c-'0')
			if n > 1<<20 {
				return 0, 0, 0, false
			}
		}
		out[i] = n
	}
	return out[0], out[1], out[2], true
}

// UserAgent returns the HTTP user agent string, derived from Version.
func UserAgent() string {
	return "Hydra/" + Version
}
