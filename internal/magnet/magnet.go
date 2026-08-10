// Package magnet turns a magnet URI into a real .torrent file.
//
// The rest of Hydra only knows how to add a .torrent: the hoard add path reads
// the file to derive the name, the multi-file flag and the save_path subfolder,
// and the qBit shim depends on those same rules. Rather than grow a second add
// path for magnets, we resolve the info dict up front and hand the existing
// path a genuine .torrent.
package magnet

import (
	"crypto/sha1"
	"encoding/base32"
	"encoding/hex"
	"fmt"
	"net/url"
	"strconv"
	"strings"
)

// Link is a parsed magnet URI.
type Link struct {
	// InfoHash is the 40-char lowercase hex info hash.
	InfoHash string
	// DisplayName is the `dn` hint. Advisory only: the real name lives in the
	// info dict, and a magnet can lie about it.
	DisplayName string
	// Trackers are the `tr` entries, in the order given.
	Trackers []string
}

// IsMagnet reports whether s looks like a magnet URI.
func IsMagnet(s string) bool {
	return strings.HasPrefix(strings.ToLower(strings.TrimSpace(s)), "magnet:")
}

// Parse reads a magnet URI. It requires a BitTorrent info hash (`xt=urn:btih:`)
// in either hex or base32; everything else is optional.
func Parse(raw string) (*Link, error) {
	raw = strings.TrimSpace(raw)
	if !IsMagnet(raw) {
		return nil, fmt.Errorf("magnet: not a magnet URI")
	}
	// A magnet's payload is a query string, but url.Parse leaves it in Opaque
	// because there is no "//" authority.
	u, err := url.Parse(raw)
	if err != nil {
		return nil, fmt.Errorf("magnet: parse: %w", err)
	}
	q := u.Query()
	if len(q) == 0 && u.Opaque != "" {
		if q, err = url.ParseQuery(strings.TrimPrefix(u.Opaque, "?")); err != nil {
			return nil, fmt.Errorf("magnet: parse query: %w", err)
		}
	}

	link := &Link{DisplayName: q.Get("dn")}
	for _, xt := range q["xt"] {
		ih, err := infoHashFromXT(xt)
		if err != nil {
			continue
		}
		link.InfoHash = ih
		break
	}
	if link.InfoHash == "" {
		return nil, fmt.Errorf("magnet: no usable xt=urn:btih: info hash")
	}
	for _, tr := range q["tr"] {
		if tr = strings.TrimSpace(tr); tr != "" {
			link.Trackers = append(link.Trackers, tr)
		}
	}
	return link, nil
}

// infoHashFromXT accepts both encodings BEP 9 magnets use in the wild: 40 hex
// chars, or 32 base32 chars (which some trackers still emit).
func infoHashFromXT(xt string) (string, error) {
	const prefix = "urn:btih:"
	if !strings.HasPrefix(strings.ToLower(xt), prefix) {
		return "", fmt.Errorf("not a btih urn")
	}
	v := xt[len(prefix):]
	switch len(v) {
	case 40:
		if _, err := hex.DecodeString(v); err != nil {
			return "", fmt.Errorf("bad hex info hash: %w", err)
		}
		return strings.ToLower(v), nil
	case 32:
		b, err := base32.StdEncoding.DecodeString(strings.ToUpper(v))
		if err != nil {
			return "", fmt.Errorf("bad base32 info hash: %w", err)
		}
		return hex.EncodeToString(b), nil
	default:
		return "", fmt.Errorf("info hash is %d chars, want 40 or 32", len(v))
	}
}

// BuildTorrent wraps a raw info dict into a complete .torrent file.
//
// infoDict must be the exact bytes fetched from the swarm: it is spliced in
// verbatim, never re-encoded. Re-encoding would risk a different byte order or
// dropped non-standard keys, and the info hash is the SHA-1 of these bytes --
// change one and the torrent stops being the torrent that was asked for.
func BuildTorrent(infoDict []byte, trackers []string) ([]byte, error) {
	if len(infoDict) == 0 {
		return nil, fmt.Errorf("magnet: empty info dict")
	}
	if infoDict[0] != 'd' || infoDict[len(infoDict)-1] != 'e' {
		return nil, fmt.Errorf("magnet: info dict is not a bencoded dict")
	}

	var b strings.Builder
	b.WriteByte('d')
	// Bencode dict keys must be in lexicographic order:
	// "announce" < "announce-list" < "info".
	if len(trackers) > 0 {
		b.WriteString(bstr("announce"))
		b.WriteString(bstr(trackers[0]))
		b.WriteString(bstr("announce-list"))
		b.WriteByte('l')
		for _, t := range trackers {
			// Each tier is its own list; one tracker per tier keeps the
			// announce order the magnet gave us.
			b.WriteByte('l')
			b.WriteString(bstr(t))
			b.WriteByte('e')
		}
		b.WriteByte('e')
	}
	b.WriteString(bstr("info"))

	out := make([]byte, 0, b.Len()+len(infoDict)+1)
	out = append(out, b.String()...)
	out = append(out, infoDict...)
	out = append(out, 'e')
	return out, nil
}

// InfoHashOf returns the hex info hash of a raw info dict.
func InfoHashOf(infoDict []byte) string {
	sum := sha1.Sum(infoDict)
	return hex.EncodeToString(sum[:])
}

// Verify checks that an info dict really is the one that was asked for. Typhon
// checks this too before handing the bytes over; doing it again here is cheap
// and keeps a bad dict from ever reaching the disk.
func Verify(infoDict []byte, wantInfoHash string) error {
	got := InfoHashOf(infoDict)
	if !strings.EqualFold(got, wantInfoHash) {
		return fmt.Errorf("magnet: info dict hashes to %s, expected %s", got, wantInfoHash)
	}
	return nil
}

// bstr bencodes a byte string.
func bstr(s string) string {
	return strconv.Itoa(len(s)) + ":" + s
}
