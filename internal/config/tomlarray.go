package config

import (
	"fmt"
	"strings"
)

// Editing [[array]] blocks in place.
//
// SetTOMLValue and SetTOMLTable address a table by NAME, which an array of
// tables does not have: every [[agent]] header is the same string, and what
// tells them apart is a key inside the block. So these address a block by a
// SELECTOR -- name = "seedbox-de" -- and edit between that header and the next
// one.
//
// Line-based like the rest of this file, and for the same reason: the config is
// a document a human wrote and comes back to. Round-tripping it through a TOML
// marshaller would produce a valid file with every comment and every ordering
// decision erased, which is a worse outcome than not offering the edit.

// arrayHeaderName returns the array a "[[x]]" line declares, or "".
func arrayHeaderName(ln string) string {
	t := strings.TrimSpace(ln)
	if strings.HasPrefix(t, "[[") && strings.HasSuffix(t, "]]") {
		return strings.TrimSpace(t[2 : len(t)-2])
	}
	return ""
}

// blockRange returns the [start, end) line span of the [[array]] block whose
// selectorKey equals selectorValue, and whether it was found. start is the
// header line itself.
//
// The span stops at the next header of ANY kind, including a nested
// [[array.sub]]: a key appended past one would land in the sub-table and mean
// something else entirely.
func blockRange(lines []string, array, selectorKey, selectorValue string) (start, end int, found bool) {
	want := selectorKey + " ="
	for i, ln := range lines {
		if arrayHeaderName(ln) != array {
			continue
		}
		// Scan this block for the selector, stopping at the next header.
		j := i + 1
		match := false
		for ; j < len(lines); j++ {
			if isTableHeader(lines[j]) {
				break
			}
			// Strip an inline comment BEFORE comparing. Without this a block
			// whose selector line carries a note -- name = "de-1"  # frankfurt
			// -- is invisible to the editor, and setting a key on it appends a
			// SECOND entry with the same name instead of editing the first.
			raw := lines[j]
			if c := findInlineComment(raw); c >= 0 {
				raw = raw[:c]
			}
			t := strings.TrimSpace(raw)
			if !strings.HasPrefix(t, want) {
				continue
			}
			if v := strings.TrimSpace(t[len(want):]); v == `"`+selectorValue+`"` || v == "'"+selectorValue+"'" {
				match = true
			}
		}
		if match {
			return i, j, true
		}
	}
	return 0, 0, false
}

// SetTOMLArrayTable sets keys inside the [[array]] block selected by
// selectorKey = selectorValue, creating the block if no such entry exists.
// Values must already be encoded TOML scalars.
//
// Keys already present are rewritten where they stand, so comments and ordering
// survive; missing ones are inserted just under the header rather than at the
// end of the block, which keeps them above any nested [[array.sub]].
func SetTOMLArrayTable(doc, array, selectorKey, selectorValue string, kv [][2]string) (string, error) {
	if strings.TrimSpace(array) == "" || strings.TrimSpace(selectorKey) == "" {
		return "", fmt.Errorf("toml: empty array or selector key")
	}
	lines := strings.Split(doc, "\n")
	start, end, found := blockRange(lines, array, selectorKey, selectorValue)
	if !found {
		// A new entry goes at the end of the file: the only place we are sure
		// not to land inside somebody else's block.
		added := []string{"", "[[" + array + "]]", selectorKey + " = " + QuoteTOMLKey(selectorValue)}
		for _, p := range kv {
			if p[0] == selectorKey {
				continue
			}
			added = append(added, p[0]+" = "+p[1])
		}
		return strings.TrimRight(doc, "\n") + "\n" + strings.Join(added, "\n") + "\n", nil
	}

	out := append([]string(nil), lines...)
	var missing [][2]string
	for _, p := range kv {
		if p[0] == selectorKey {
			continue // the selector identifies the block; rewriting it would move the entry
		}
		done := false
		for i := start + 1; i < end; i++ {
			t := strings.TrimSpace(out[i])
			if !strings.HasPrefix(t, p[0]+" =") && !strings.HasPrefix(t, p[0]+"=") {
				continue
			}
			indent := out[i][:len(out[i])-len(strings.TrimLeft(out[i], " \t"))]
			if c := findInlineComment(out[i]); c >= 0 {
				out[i] = indent + p[0] + " = " + p[1] + " " + strings.TrimSpace(out[i][c:])
			} else {
				out[i] = indent + p[0] + " = " + p[1]
			}
			done = true
			break
		}
		if !done {
			missing = append(missing, p)
		}
	}
	if len(missing) == 0 {
		return strings.Join(out, "\n"), nil
	}
	ins := make([]string, 0, len(missing))
	for _, p := range missing {
		ins = append(ins, p[0]+" = "+p[1])
	}
	res := make([]string, 0, len(out)+len(ins))
	res = append(res, out[:start+1]...)
	res = append(res, ins...)
	res = append(res, out[start+1:]...)
	return strings.Join(res, "\n"), nil
}

// blockEndIncludingNested extends a block past its own sub-tables, to the next
// header that does NOT belong to it.
//
// Insertion and deletion need different ends, which is not obvious and cost a
// broken document to find. Inserting a key must stop at the first header of any
// kind, so the key lands above a nested [[agent.engine]] rather than inside it.
// Deleting must swallow those nested blocks: leaving them behind orphans them
// under whatever entry follows, and the file stops parsing entirely --
// "key table already exists as a agent, but should be an array table".
func blockEndIncludingNested(lines []string, array string, start int) int {
	nested := array + "."
	for j := start + 1; j < len(lines); j++ {
		if !isTableHeader(lines[j]) {
			continue
		}
		name := arrayHeaderName(lines[j])
		if name == "" {
			name = sectionName(lines[j])
		}
		if strings.HasPrefix(name, nested) {
			continue // a sub-table of this entry: it goes with it
		}
		return j
	}
	return len(lines)
}

// DeleteTOMLArrayTable removes the whole [[array]] block selected by
// selectorKey = selectorValue. Idempotent: removing an entry that is not there
// succeeds, so deleting an agent twice is not an error.
func DeleteTOMLArrayTable(doc, array, selectorKey, selectorValue string) string {
	lines := strings.Split(doc, "\n")
	start, _, found := blockRange(lines, array, selectorKey, selectorValue)
	if !found {
		return doc
	}
	end := blockEndIncludingNested(lines, array, start)
	// Swallow one blank line before the header so removing an entry does not
	// leave a growing gap where it used to be.
	for start > 0 && strings.TrimSpace(lines[start-1]) == "" {
		start--
		break
	}
	res := append([]string(nil), lines[:start]...)
	res = append(res, lines[end:]...)
	return strings.Join(res, "\n")
}
