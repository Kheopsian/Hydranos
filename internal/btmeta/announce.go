// Package btmeta edits the parts of a .torrent that are safe to edit.
//
// The tracker list lives in the top-level dict, next to `info` but outside it.
// That is what makes editing it safe at all: the infohash is the SHA-1 of the
// `info` dict's bencoded bytes, so as long as those bytes are copied through
// untouched, a torrent keeps its identity while its trackers change.
//
// "Copied through untouched" is the whole point of this package. Decoding the
// info dict and re-encoding it would round-trip through Go's idea of key order
// and integer formatting, and any disagreement with the original — a key out of
// order, a leading zero — changes the hash and silently turns the torrent into
// a different one. So values are kept as raw byte spans and only the two
// announce keys are ever rebuilt.
package btmeta

import (
	"errors"
	"fmt"
	"sort"
)

// entry is one top-level key and the exact bytes of its value.
type entry struct {
	key string
	val []byte // raw, exactly as it appeared in the input
}

// SetAnnounce returns a copy of raw whose announce / announce-list keys describe
// tiers, leaving every other key — `info` above all — byte for byte identical.
//
// An empty tiers list removes both keys: a torrent that announces to nobody is
// a legitimate state, and leaving an empty announce-list behind would be a lie
// that some clients read as "one tracker with an empty URL".
func SetAnnounce(raw []byte, tiers [][]string) ([]byte, error) {
	entries, err := topLevel(raw)
	if err != nil {
		return nil, err
	}

	clean := make([][]string, 0, len(tiers))
	for _, tier := range tiers {
		urls := make([]string, 0, len(tier))
		for _, u := range tier {
			if u != "" {
				urls = append(urls, u)
			}
		}
		if len(urls) > 0 {
			clean = append(clean, urls)
		}
	}

	out := make([]entry, 0, len(entries)+2)
	for _, e := range entries {
		if e.key == "announce" || e.key == "announce-list" {
			continue
		}
		out = append(out, e)
	}
	if len(clean) > 0 {
		// `announce` carries the first URL for clients that predate BEP 12, and
		// `announce-list` carries the tiers. Writing only the second leaves old
		// clients with no tracker at all.
		out = append(out, entry{key: "announce", val: bencString(clean[0][0])})
		out = append(out, entry{key: "announce-list", val: bencTiers(clean)})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].key < out[j].key })

	buf := make([]byte, 0, len(raw)+64)
	buf = append(buf, 'd')
	for _, e := range out {
		buf = append(buf, bencString(e.key)...)
		buf = append(buf, e.val...)
	}
	buf = append(buf, 'e')
	return buf, nil
}

// Announce reads the tracker tiers out of a .torrent, using announce-list when
// present and falling back to the single announce key.
func Announce(raw []byte) ([][]string, error) {
	entries, err := topLevel(raw)
	if err != nil {
		return nil, err
	}
	byKey := map[string][]byte{}
	for _, e := range entries {
		byKey[e.key] = e.val
	}
	if v, ok := byKey["announce-list"]; ok {
		tiers, err := parseTiers(v)
		if err == nil && len(tiers) > 0 {
			return tiers, nil
		}
	}
	if v, ok := byKey["announce"]; ok {
		s, _, err := scanString(v, 0)
		if err == nil && s != "" {
			return [][]string{{s}}, nil
		}
	}
	return nil, nil
}

// InfoSpan returns the raw bytes of the `info` value, which is what the
// infohash is computed over. Callers use it to prove an edit did not touch it.
func InfoSpan(raw []byte) ([]byte, error) {
	entries, err := topLevel(raw)
	if err != nil {
		return nil, err
	}
	for _, e := range entries {
		if e.key == "info" {
			return e.val, nil
		}
	}
	return nil, errors.New("btmeta: no info dict")
}

// ── bencode scanning ────────────────────────────────────────────────────────
//
// Only enough to walk a value and record where it ends. Nothing here allocates
// a decoded tree for anything but the tracker keys.

func topLevel(raw []byte) ([]entry, error) {
	if len(raw) == 0 || raw[0] != 'd' {
		return nil, errors.New("btmeta: not a bencoded dict")
	}
	var out []entry
	i := 1
	for i < len(raw) && raw[i] != 'e' {
		key, next, err := scanString(raw, i)
		if err != nil {
			return nil, err
		}
		start := next
		end, err := scanValue(raw, start)
		if err != nil {
			return nil, err
		}
		out = append(out, entry{key: key, val: raw[start:end]})
		i = end
	}
	if i >= len(raw) {
		return nil, errors.New("btmeta: unterminated dict")
	}
	return out, nil
}

func scanString(b []byte, i int) (string, int, error) {
	j := i
	for j < len(b) && b[j] >= '0' && b[j] <= '9' {
		j++
	}
	if j == i || j >= len(b) || b[j] != ':' {
		return "", 0, fmt.Errorf("btmeta: bad string header at %d", i)
	}
	n := 0
	for _, c := range b[i:j] {
		n = n*10 + int(c-'0')
	}
	start := j + 1
	if start+n > len(b) {
		return "", 0, fmt.Errorf("btmeta: string runs past end at %d", i)
	}
	return string(b[start : start+n]), start + n, nil
}

// scanValue returns the index just past the value starting at i.
func scanValue(b []byte, i int) (int, error) {
	if i >= len(b) {
		return 0, errors.New("btmeta: unexpected end")
	}
	switch c := b[i]; {
	case c == 'i':
		j := i + 1
		for j < len(b) && b[j] != 'e' {
			j++
		}
		if j >= len(b) {
			return 0, errors.New("btmeta: unterminated int")
		}
		return j + 1, nil
	case c == 'l', c == 'd':
		j := i + 1
		for j < len(b) && b[j] != 'e' {
			var err error
			if c == 'd' {
				if _, j, err = scanString(b, j); err != nil {
					return 0, err
				}
			}
			if j, err = scanValue(b, j); err != nil {
				return 0, err
			}
		}
		if j >= len(b) {
			return 0, errors.New("btmeta: unterminated list or dict")
		}
		return j + 1, nil
	case c >= '0' && c <= '9':
		_, next, err := scanString(b, i)
		return next, err
	default:
		return 0, fmt.Errorf("btmeta: unexpected %q at %d", c, i)
	}
}

func parseTiers(v []byte) ([][]string, error) {
	if len(v) == 0 || v[0] != 'l' {
		return nil, errors.New("btmeta: announce-list is not a list")
	}
	var out [][]string
	i := 1
	for i < len(v) && v[i] != 'e' {
		if v[i] != 'l' {
			return nil, errors.New("btmeta: announce-list tier is not a list")
		}
		var tier []string
		j := i + 1
		for j < len(v) && v[j] != 'e' {
			s, next, err := scanString(v, j)
			if err != nil {
				return nil, err
			}
			if s != "" {
				tier = append(tier, s)
			}
			j = next
		}
		if j >= len(v) {
			return nil, errors.New("btmeta: unterminated tier")
		}
		if len(tier) > 0 {
			out = append(out, tier)
		}
		i = j + 1
	}
	return out, nil
}

func bencString(s string) []byte {
	return append([]byte(fmt.Sprintf("%d:", len(s))), s...)
}

func bencTiers(tiers [][]string) []byte {
	buf := []byte{'l'}
	for _, tier := range tiers {
		buf = append(buf, 'l')
		for _, u := range tier {
			buf = append(buf, bencString(u)...)
		}
		buf = append(buf, 'e')
	}
	return append(buf, 'e')
}
