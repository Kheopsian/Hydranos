package version

// Version is the single source of truth.
var Version = "3.52.0-typhon"

// PeerFingerprintID is the Azureus-style BEP-20 peer_id prefix. It MUST be
// exactly 8 bytes ("-HY####-"): the engine appends a 12-byte random suffix, so
// 8 + 12 = 20, the only length trackers accept. Encode the version as 4 digits
// (major, then minor, then patch) e.g. 3.13.9 -> "3139". If minor/patch grow
// past this, switch to base36 — never let the prefix exceed 8 bytes.
var PeerFingerprintID = "-HY3520-"

// PeerFingerprint returns the 8-byte BEP-20 peer_id prefix. Defensive: if the
// literal is ever not 8 bytes, coerce it to 8 so we never emit a 21-byte
// peer_id again (regression introduced in 3.11.0, fixed in 3.13.9).
func PeerFingerprint() string {
	b := []byte(PeerFingerprintID)
	if len(b) == 8 {
		return PeerFingerprintID
	}
	if len(b) > 8 {
		b = b[:8]
	}
	for len(b) < 8 {
		b = append(b, '-')
	}
	return string(b)
}

// UserAgent returns the HTTP user agent string, derived from Version.
func UserAgent() string {
	return "Hydra/" + Version
}
