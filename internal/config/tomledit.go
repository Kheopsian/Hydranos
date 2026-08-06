package config

import (
	"fmt"
	"strconv"
	"strings"

	toml "github.com/pelletier/go-toml/v2"
)

// findInlineComment returns the index of the first '#' in s that starts an inline
// comment (i.e. is not inside a quoted string), or -1 if there is none.
func findInlineComment(s string) int {
	inStr := false
	var q byte
	for i := 0; i < len(s); i++ {
		c := s[i]
		if inStr {
			if c == q {
				inStr = false
			}
			continue
		}
		if c == '"' || c == '\'' {
			inStr = true
			q = c
			continue
		}
		if c == '#' {
			return i
		}
	}
	return -1
}

// SetTOMLValue replaces the value of key inside [section] of a TOML document in
// place, preserving comments, blank lines, ordering and every unrelated line. An
// inline comment on the edited line is kept. section == "" targets the top-level
// table (lines before the first [header]). It returns an error if the section or
// key is not found — we never blindly append, so a missing key signals a bug.
func SetTOMLValue(doc, section, key, tomlValue string) (string, error) {
	lines := strings.Split(doc, "\n")
	cur := ""
	for i, ln := range lines {
		t := strings.TrimSpace(ln)
		if strings.HasPrefix(t, "[") && strings.HasSuffix(t, "]") && !strings.HasPrefix(t, "[[") {
			cur = strings.TrimSpace(t[1 : len(t)-1])
			continue
		}
		if cur != section || strings.HasPrefix(t, "#") {
			continue
		}
		eq := strings.Index(ln, "=")
		if eq < 0 || strings.TrimSpace(ln[:eq]) != key {
			continue
		}
		indent := ln[:len(ln)-len(strings.TrimLeft(ln, " \t"))]
		comment := ""
		if ci := findInlineComment(ln[eq+1:]); ci >= 0 {
			comment = "  " + strings.TrimSpace(ln[eq+1+ci:])
		}
		lines[i] = fmt.Sprintf("%s%s = %s%s", indent, key, tomlValue, comment)
		return strings.Join(lines, "\n"), nil
	}
	return "", fmt.Errorf("toml: key %q not found in section %q", key, section)
}

// ParseTOMLMap decodes a TOML document into a generic section->key map, used to
// expose the whole config to the settings UI and to validate an edited document.
func ParseTOMLMap(data []byte) (map[string]interface{}, error) {
	var m map[string]interface{}
	if err := toml.Unmarshal(data, &m); err != nil {
		return nil, err
	}
	return m, nil
}

// ValidateTyped ensures the document decodes into the typed HydraConfig — the
// same strict decode the daemon performs at boot (config.Load). It catches type
// mismatches that ParseTOMLMap misses: a scalar written where an array/map is
// expected stays valid *generic* TOML but makes the daemon fail to start. The
// settings POST handler runs this before writing so a save can never brick boot.
func ValidateTyped(data []byte) error {
	var cfg HydraConfig
	return toml.Unmarshal(data, &cfg)
}

// isTableHeader reports whether ln declares a table ("[x]" or "[[x]]").
func isTableHeader(ln string) bool {
	t := strings.TrimSpace(ln)
	return strings.HasPrefix(t, "[") && strings.HasSuffix(t, "]")
}

// sectionName returns the table a header line declares ("[a.b]" -> "a.b"), or ""
// for anything else — including an array-of-tables header, which no caller here
// addresses by name.
func sectionName(ln string) string {
	t := strings.TrimSpace(ln)
	if isTableHeader(ln) && !strings.HasPrefix(t, "[[") {
		return strings.TrimSpace(t[1 : len(t)-1])
	}
	return ""
}

// QuoteTOMLKey renders s as a quoted TOML key. Tracker hosts contain dots, and a
// bare dot is the table-path separator, so the host must be quoted to stay a
// single key rather than becoming a nested table.
func QuoteTOMLKey(s string) string { return strconv.Quote(s) }

// SetTOMLTable sets keys inside [section], creating the section and any missing
// key. Unlike SetTOMLValue it is additive on purpose: it backs the tracker
// editor, whose sections are named after a tracker host and therefore cannot
// pre-exist in a shipped config. Untouched lines keep their place and comments;
// a key already present is rewritten where it stands. Values must already be
// encoded TOML scalars.
func SetTOMLTable(doc, section string, kv [][2]string) (string, error) {
	if strings.TrimSpace(section) == "" {
		return "", fmt.Errorf("toml: empty section")
	}
	out := doc
	var missing [][2]string
	for _, p := range kv {
		next, err := SetTOMLValue(out, section, p[0], p[1])
		if err != nil {
			missing = append(missing, p)
			continue
		}
		out = next
	}
	if len(missing) == 0 {
		return out, nil
	}
	added := make([]string, 0, len(missing))
	for _, p := range missing {
		added = append(added, p[0]+" = "+p[1])
	}
	lines := strings.Split(out, "\n")
	for i, ln := range lines {
		if sectionName(ln) != section {
			continue
		}
		// The section exists but these keys do not: insert them under its header.
		res := make([]string, 0, len(lines)+len(added))
		res = append(res, lines[:i+1]...)
		res = append(res, added...)
		res = append(res, lines[i+1:]...)
		return strings.Join(res, "\n"), nil
	}
	// A brand new table goes at the end of the file — the only position where we
	// are sure we are not landing inside somebody else's table.
	return strings.TrimRight(out, "\n") + "\n\n[" + section + "]\n" +
		strings.Join(added, "\n") + "\n", nil
}

// DeleteTOMLKey removes key from [section]. Idempotent: deleting something that
// is not there is a no-op, so clearing an override the config never carried
// succeeds rather than erroring.
func DeleteTOMLKey(doc, section, key string) string {
	lines := strings.Split(doc, "\n")
	out := make([]string, 0, len(lines))
	cur := ""
	for _, ln := range lines {
		if isTableHeader(ln) {
			// "" for an array-of-tables header, which therefore never matches a
			// section we were asked about — its keys are not ours to delete.
			cur = sectionName(ln)
			out = append(out, ln)
			continue
		}
		if cur == section && !strings.HasPrefix(strings.TrimSpace(ln), "#") {
			if eq := strings.Index(ln, "="); eq >= 0 && strings.TrimSpace(ln[:eq]) == key {
				continue
			}
		}
		out = append(out, ln)
	}
	return strings.Join(out, "\n")
}

// DeleteTOMLTable removes the [section] header and every line it owns, up to the
// next header of any kind. Idempotent, like DeleteTOMLKey.
func DeleteTOMLTable(doc, section string) string {
	if strings.TrimSpace(section) == "" {
		return doc
	}
	lines := strings.Split(doc, "\n")
	out := make([]string, 0, len(lines))
	dropping := false
	for _, ln := range lines {
		if isTableHeader(ln) {
			dropping = sectionName(ln) == section
		}
		if dropping {
			continue
		}
		out = append(out, ln)
	}
	return strings.TrimRight(strings.Join(out, "\n"), "\n") + "\n"
}

// PruneEmptyTable removes a [section] header whose body holds nothing but blank
// lines — what clearing the last override leaves behind. An empty table and an
// absent one mean the same thing to the decoder, so the file should not keep a
// header that suggests otherwise. A body with comments is left alone: those are
// the user's words, not ours.
func PruneEmptyTable(doc, section string) string {
	if strings.TrimSpace(section) == "" {
		return doc
	}
	lines := strings.Split(doc, "\n")
	start := -1
	for i, ln := range lines {
		if sectionName(ln) == section {
			start = i
			break
		}
	}
	if start < 0 {
		return doc
	}
	end := len(lines)
	for i := start + 1; i < len(lines); i++ {
		if isTableHeader(lines[i]) {
			end = i
			break
		}
		if strings.TrimSpace(lines[i]) != "" {
			return doc // a key or a comment still lives here
		}
	}
	res := make([]string, 0, len(lines))
	res = append(res, lines[:start]...)
	res = append(res, lines[end:]...)
	return strings.TrimRight(strings.Join(res, "\n"), "\n") + "\n"
}
