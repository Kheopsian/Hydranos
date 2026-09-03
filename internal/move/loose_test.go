package move

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

// A payload dropped straight into a category directory -- the shape an
// rtorrent or Transmission import leaves behind -- shares that directory with
// every other torrent in the category. These tests exist because the obvious
// implementation moves the directory, and that takes the whole category.

func writeLooseFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func readLooseFile(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("reading %s: %v", path, err)
	}
	return string(b)
}

// looseFixture builds two category directories, each already holding somebody
// else's torrent, plus the payload under test sitting loose in the first.
func looseFixture(t *testing.T) (catA, catB string) {
	t.Helper()
	root := t.TempDir()
	catA = filepath.Join(root, "Done")
	catB = filepath.Join(root, "Films-Moved")
	writeLooseFile(t, filepath.Join(catA, "ours.mkv"), "ours")
	writeLooseFile(t, filepath.Join(catA, "someone-else.mkv"), "not ours")
	writeLooseFile(t, filepath.Join(catB, "already-here.mkv"), "already here")
	return catA, catB
}

func assertCategorySurvived(t *testing.T, catA, catB string) {
	t.Helper()
	if _, err := os.Stat(catA); err != nil {
		t.Fatalf("the category directory %s was moved or removed: %v", catA, err)
	}
	if got := readLooseFile(t, filepath.Join(catA, "someone-else.mkv")); got != "not ours" {
		t.Fatalf("another torrent's file in the category was disturbed: %q", got)
	}
	if got := readLooseFile(t, filepath.Join(catB, "already-here.mkv")); got != "already here" {
		t.Fatalf("a file already in the target category was disturbed: %q", got)
	}
	if _, err := os.Stat(filepath.Join(catB, filepath.Base(catA))); err == nil {
		t.Fatalf("the category directory was moved inside %s", catB)
	}
}

func TestLooseMoveTakesOnlyItsOwnFilesAndLeavesTheCategoryAlone(t *testing.T) {
	catA, catB := looseFixture(t)

	p, err := InspectLoose(catA, catB, "af92d708", []string{"ours.mkv"})
	if err != nil {
		t.Fatalf("InspectLoose: %v", err)
	}
	if !p.Loose {
		t.Fatal("the plan is not marked loose, so Execute will move the directory")
	}
	if err := Execute(context.Background(), p, Options{}); err != nil {
		t.Fatalf("Execute: %v", err)
	}

	assertCategorySurvived(t, catA, catB)
	if got := readLooseFile(t, filepath.Join(catB, "ours.mkv")); got != "ours" {
		t.Fatalf("the payload did not arrive intact: %q", got)
	}
	if _, err := os.Stat(filepath.Join(catA, "ours.mkv")); !os.IsNotExist(err) {
		t.Fatal("the payload is still at the source")
	}
}

func TestLooseCopyLeavesTheCategoryAloneAndCleansItsStaging(t *testing.T) {
	catA, catB := looseFixture(t)

	p, err := InspectLoose(catA, catB, "af92d708", []string{"ours.mkv"})
	if err != nil {
		t.Fatalf("InspectLoose: %v", err)
	}
	// One TempDir is one filesystem, so the copy path has to be asked for. It
	// is the path worth testing: it deletes the source files itself rather
	// than letting a rename do it, so a mistake there loses the payload.
	p.SameFS = false
	if err := Execute(context.Background(), p, Options{}); err != nil {
		t.Fatalf("Execute: %v", err)
	}

	assertCategorySurvived(t, catA, catB)
	if got := readLooseFile(t, filepath.Join(catB, "ours.mkv")); got != "ours" {
		t.Fatalf("the payload did not arrive intact: %q", got)
	}
	if _, err := os.Stat(filepath.Join(catA, "ours.mkv")); !os.IsNotExist(err) {
		t.Fatal("the payload is still at the source")
	}
	if _, err := os.Stat(p.Staging); !os.IsNotExist(err) {
		t.Fatalf("the staging directory %s was left behind in the category", p.Staging)
	}
}

func TestLooseMoveRefusesToOverwriteAFileAlreadyInTheTargetCategory(t *testing.T) {
	catA, catB := looseFixture(t)
	writeLooseFile(t, filepath.Join(catA, "already-here.mkv"), "ours, same name")

	p, err := InspectLoose(catA, catB, "af92d708", []string{"already-here.mkv"})
	if err != nil {
		t.Fatalf("InspectLoose: %v", err)
	}
	if err := p.Check(true); !errors.Is(err, ErrTargetFileExists) {
		t.Fatalf("a name already taken in the target category was not refused: %v", err)
	}
	if got := readLooseFile(t, filepath.Join(catB, "already-here.mkv")); got != "already here" {
		t.Fatalf("the file in the way was touched: %q", got)
	}
}

func TestLooseMovePutsBackWhatItMovedWhenALaterFileIsBlocked(t *testing.T) {
	catA, catB := looseFixture(t)
	writeLooseFile(t, filepath.Join(catA, "second.mkv"), "second")

	p, err := InspectLoose(catA, catB, "af92d708", []string{"ours.mkv", "second.mkv"})
	if err != nil {
		t.Fatalf("InspectLoose: %v", err)
	}
	// The collision appears after the plan was made. A plan is a snapshot,
	// which is exactly why the swap re-checks instead of trusting it.
	writeLooseFile(t, filepath.Join(catB, "second.mkv"), "somebody else's second")

	afterSwap := false
	err = Execute(context.Background(), p, Options{AfterSwap: func() error { afterSwap = true; return nil }})
	if !errors.Is(err, ErrTargetFileExists) {
		t.Fatalf("the move was not refused: %v", err)
	}
	if afterSwap {
		t.Fatal("the engine was repointed at a move that did not happen")
	}
	if got := readLooseFile(t, filepath.Join(catA, "ours.mkv")); got != "ours" {
		t.Fatalf("the first file was not put back at the source: %q", got)
	}
	if got := readLooseFile(t, filepath.Join(catA, "second.mkv")); got != "second" {
		t.Fatalf("the second file left the source: %q", got)
	}
	if got := readLooseFile(t, filepath.Join(catB, "second.mkv")); got != "somebody else's second" {
		t.Fatalf("another torrent's file was overwritten: %q", got)
	}
}

func TestLooseMoveRejectsAFileListThatEscapesTheDirectory(t *testing.T) {
	catA, catB := looseFixture(t)
	// The escape target has to exist, otherwise the plain "is this file
	// there?" check refuses these paths and the guard is never exercised.
	writeLooseFile(t, filepath.Join(filepath.Dir(catA), "outside.mkv"), "outside")
	for _, bad := range []string{"../outside.mkv", "sub/../../outside.mkv"} {
		if _, err := InspectLoose(catA, catB, "af92d708", []string{bad}); err == nil {
			t.Fatalf("%q was accepted as a payload path, so a torrent can name files outside its own directory", bad)
		}
	}
	if _, err := InspectLoose(catA, catB, "af92d708", []string{"/etc/hostname"}); err == nil {
		t.Fatal("an absolute path was accepted as a payload path")
	}
}

func TestLooseMoveNeedsTheTorrentsOwnFileList(t *testing.T) {
	catA, catB := looseFixture(t)
	if _, err := InspectLoose(catA, catB, "af92d708", nil); err == nil {
		t.Fatal("a loose move with no file list was accepted; there is nothing to tell the payload from the rest of the category")
	}
}
