package config

import (
	"strings"
	"testing"
)

const agentsDoc = `# the fleet
[race]
max_connections = 4000

[[agent]]
name = "de-1"          # frankfurt
addr = "10.0.0.5:9090"
  [[agent.engine]]
  id = "race-0"
  announce_proxy = "socks5h://x@127.0.0.1:1080"

[[agent]]
name = "fr-1"
addr = "10.0.0.6:9090"
`

func TestSetArrayTableEditsTheSelectedBlock(t *testing.T) {
	out, err := SetTOMLArrayTable(agentsDoc, "agent", "name", "fr-1", [][2]string{
		{"addr", `"10.0.0.9:9090"`},
	})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, `10.0.0.9:9090`) {
		t.Fatal("the selected block was not updated")
	}
	if !strings.Contains(out, `addr = "10.0.0.5:9090"`) {
		t.Error("the OTHER block was modified: selecting by name matched the wrong entry")
	}
}

// A comment on the edited line has to survive. The config is a document someone
// wrote; an editor that strips their notes is one they stop using.
func TestSetArrayTableKeepsInlineComments(t *testing.T) {
	out, err := SetTOMLArrayTable(agentsDoc, "agent", "name", "de-1", [][2]string{
		{"token", `"s3cret"`},
	})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out, "# frankfurt") {
		t.Error("an inline comment was lost")
	}
}

// A new key must land under the header, ABOVE any nested [[agent.engine]].
// Appended after one it would become a key of the sub-table and mean something
// else entirely -- a per-engine override instead of an agent setting.
func TestNewKeyLandsAboveNestedBlocks(t *testing.T) {
	out, err := SetTOMLArrayTable(agentsDoc, "agent", "name", "de-1", [][2]string{
		{"role", `"race"`},
	})
	if err != nil {
		t.Fatal(err)
	}
	iRole := strings.Index(out, `role = "race"`)
	iNested := strings.Index(out, "[[agent.engine]]")
	if iRole < 0 || iNested < 0 || iRole > iNested {
		t.Fatalf("role landed at %d, nested block at %d: the key fell into the sub-table", iRole, iNested)
	}
}

// An unknown selector appends a new entry rather than editing a random one.
func TestUnknownSelectorAppendsANewEntry(t *testing.T) {
	out, err := SetTOMLArrayTable(agentsDoc, "agent", "name", "local-race", [][2]string{
		{"role", `"race"`}, {"listen_port", "16171"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(out, "[[agent]]") != 3 {
		t.Fatalf("%d entries, want 3", strings.Count(out, "[[agent]]"))
	}
	if !strings.Contains(out, `name = "local-race"`) || !strings.Contains(out, "listen_port = 16171") {
		t.Error("the new entry is incomplete")
	}
	// It must still parse, or we have written a valid-looking corrupt file.
	if _, err := ParseTOMLMap([]byte(out)); err != nil {
		t.Fatalf("the edited document no longer parses: %v", err)
	}
}

// The selector itself must never be rewritten: it identifies the block, and
// changing it would silently move the entry somewhere else.
func TestSelectorKeyIsNeverRewritten(t *testing.T) {
	out, _ := SetTOMLArrayTable(agentsDoc, "agent", "name", "fr-1", [][2]string{
		{"name", `"somewhere-else"`}, {"token", `"t"`},
	})
	if strings.Contains(out, "somewhere-else") {
		t.Error("the selector was rewritten: the entry would have changed identity")
	}
	if !strings.Contains(out, `token = "t"`) {
		t.Error("the other key was not applied")
	}
}

func TestDeleteArrayTableRemovesOnlyThatEntry(t *testing.T) {
	out := DeleteTOMLArrayTable(agentsDoc, "agent", "name", "de-1")
	if strings.Contains(out, `name = "de-1"`) {
		t.Error("the entry survived")
	}
	if strings.Contains(out, "[[agent.engine]]") {
		t.Error("its nested block was left behind, orphaned under the next agent")
	}
	if !strings.Contains(out, `name = "fr-1"`) {
		t.Error("the other entry was removed too")
	}
	if _, err := ParseTOMLMap([]byte(out)); err != nil {
		t.Fatalf("document no longer parses: %v", err)
	}
	// Idempotent.
	if again := DeleteTOMLArrayTable(out, "agent", "name", "de-1"); again != out {
		t.Error("deleting an absent entry changed the document")
	}
}
