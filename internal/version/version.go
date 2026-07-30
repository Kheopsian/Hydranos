package version

// Version is the single source of truth.
var Version = "3.9.1-typhon"

// PeerFingerprintID is the 8-byte BEP-20 peer_id prefix. Azureus-style
// -HY<MMmpb>- where each digit is one char. Bumped when Version changes.
var PeerFingerprintID = "-HY3910-"

// PeerFingerprint returns the BEP-20 peer_id prefix.
func PeerFingerprint() string {
	return PeerFingerprintID
}

// UserAgent returns the HTTP user agent string, derived from Version.
func UserAgent() string {
	return "Hydra/" + Version
}
