package config

import (
	"fmt"
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
