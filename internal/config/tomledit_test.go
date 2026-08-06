package config

import "strings"
import "testing"

const sample = `# top comment
api_key = "old"

[race]
listen_port = 16171
active_downloads = 30  # engine slots
choking_algorithm = "rate_based"

[race.custom_choking]
enabled = true
`

func TestSetTOMLValue_Scalar(t *testing.T) {
	out, err := SetTOMLValue(sample, "race", "active_downloads", "200")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "active_downloads = 200  # engine slots") {
		t.Fatalf("value not replaced / comment lost:\n%s", out)
	}
	// unrelated lines preserved
	for _, want := range []string{"# top comment", "listen_port = 16171", "choking_algorithm = \"rate_based\"", "[race.custom_choking]"} {
		if !strings.Contains(out, want) {
			t.Fatalf("unrelated line lost: %q", want)
		}
	}
}

func TestSetTOMLValue_TopLevelAndNested(t *testing.T) {
	out, _ := SetTOMLValue(sample, "", "api_key", `"new"`)
	if !strings.Contains(out, `api_key = "new"`) {
		t.Fatalf("top-level not set:\n%s", out)
	}
	out2, _ := SetTOMLValue(sample, "race.custom_choking", "enabled", "false")
	if !strings.Contains(out2, "enabled = false") {
		t.Fatalf("nested section not set:\n%s", out2)
	}
}

func TestSetTOMLValue_WrongSection(t *testing.T) {
	// listen_port exists in [race], not top-level -> must error, never touch it
	if _, err := SetTOMLValue(sample, "", "listen_port", "9"); err == nil {
		t.Fatal("expected error for key in wrong section")
	}
	if _, err := SetTOMLValue(sample, "race", "nonexistent", "1"); err == nil {
		t.Fatal("expected error for missing key")
	}
}

func TestSetTOMLTable_CreatesSection(t *testing.T) {
	out, err := SetTOMLTable(sample, "announce_clients."+QuoteTOMLKey("t.myanonamouse.net"),
		[][2]string{{"peer_id_prefix", `"-qB5220-"`}, {"user_agent", `"qBittorrent/5.2.2"`}})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, `[announce_clients."t.myanonamouse.net"]`) {
		t.Fatalf("section not created:\n%s", out)
	}
	if !strings.Contains(out, `peer_id_prefix = "-qB5220-"`) || !strings.Contains(out, `user_agent = "qBittorrent/5.2.2"`) {
		t.Fatalf("keys not written:\n%s", out)
	}
	// the host stays one key, not a nested table
	var cfg HydraConfig
	if err := ValidateTyped([]byte(out)); err != nil {
		t.Fatalf("result does not decode: %v", err)
	}
	if _, err := ParseTOMLMap([]byte(out)); err != nil {
		t.Fatalf("result does not parse: %v", err)
	}
	_ = cfg
	// unrelated content survives
	if !strings.Contains(out, "# top comment") || !strings.Contains(out, "active_downloads = 30  # engine slots") {
		t.Fatalf("unrelated line lost:\n%s", out)
	}
}

func TestSetTOMLTable_UpdatesExistingAndAddsMissing(t *testing.T) {
	first, _ := SetTOMLTable(sample, "announce_clients."+QuoteTOMLKey("h.example"),
		[][2]string{{"peer_id_prefix", `"-qB5220-"`}})
	second, err := SetTOMLTable(first, "announce_clients."+QuoteTOMLKey("h.example"),
		[][2]string{{"peer_id_prefix", `"-DE13F0-"`}, {"user_agent", `"Deluge"`}})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(second, `[announce_clients."h.example"]`) != 1 {
		t.Fatalf("section duplicated:\n%s", second)
	}
	if strings.Contains(second, "-qB5220-") || !strings.Contains(second, "-DE13F0-") {
		t.Fatalf("existing key not updated in place:\n%s", second)
	}
	if !strings.Contains(second, `user_agent = "Deluge"`) {
		t.Fatalf("missing key not inserted:\n%s", second)
	}
}

func TestDeleteTOMLTable_RoundTrip(t *testing.T) {
	sec := "announce_clients." + QuoteTOMLKey("h.example")
	with, _ := SetTOMLTable(sample, sec, [][2]string{{"peer_id_prefix", `"-qB5220-"`}})
	back := DeleteTOMLTable(with, sec)
	if strings.Contains(back, "announce_clients") || strings.Contains(back, "-qB5220-") {
		t.Fatalf("table not removed:\n%s", back)
	}
	if !strings.Contains(back, "listen_port = 16171") || !strings.Contains(back, "[race.custom_choking]") {
		t.Fatalf("removal ate unrelated lines:\n%s", back)
	}
	// idempotent
	if again := DeleteTOMLTable(back, sec); again != back {
		t.Fatalf("second delete changed the document")
	}
}

// A deleted table must stop at the next header, including an array-of-tables
// one: [[agent]] blocks live at the end of the shipped config.
func TestDeleteTOMLTable_StopsAtArrayOfTables(t *testing.T) {
	doc := "[announce_passkeys]\n\"h.example\" = \"k\"\n\n[[agent]]\nname = \"a1\"\n"
	out := DeleteTOMLTable(doc, "announce_passkeys")
	if !strings.Contains(out, "[[agent]]") || !strings.Contains(out, `name = "a1"`) {
		t.Fatalf("array-of-tables block eaten:\n%s", out)
	}
	if strings.Contains(out, "announce_passkeys") {
		t.Fatalf("table not removed:\n%s", out)
	}
}

func TestDeleteTOMLKey(t *testing.T) {
	doc := "[announce_passkeys]\n\"a.example\" = \"k1\"\n\"b.example\" = \"k2\"\n"
	out := DeleteTOMLKey(doc, "announce_passkeys", QuoteTOMLKey("a.example"))
	if strings.Contains(out, "a.example") || !strings.Contains(out, "b.example") {
		t.Fatalf("wrong key removed:\n%s", out)
	}
	if same := DeleteTOMLKey(out, "announce_passkeys", QuoteTOMLKey("a.example")); same != out {
		t.Fatal("delete of an absent key changed the document")
	}
}

func TestPruneEmptyTable(t *testing.T) {
	doc := "[race]\nlisten_port = 1\n\n[announce_passkeys]\n\n[[agent]]\nname = \"a1\"\n"
	out := PruneEmptyTable(doc, "announce_passkeys")
	if strings.Contains(out, "announce_passkeys") {
		t.Fatalf("empty table kept:\n%s", out)
	}
	if !strings.Contains(out, "[[agent]]") || !strings.Contains(out, "listen_port = 1") {
		t.Fatalf("prune ate a neighbour:\n%s", out)
	}
	// a table that still holds a key, or a comment, is untouched
	kept := "[announce_passkeys]\n\"h\" = \"k\"\n"
	if PruneEmptyTable(kept, "announce_passkeys") != kept {
		t.Fatal("pruned a non-empty table")
	}
	commented := "[announce_passkeys]\n# keep me\n"
	if PruneEmptyTable(commented, "announce_passkeys") != commented {
		t.Fatal("pruned a table holding a user comment")
	}
}
