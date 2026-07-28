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
