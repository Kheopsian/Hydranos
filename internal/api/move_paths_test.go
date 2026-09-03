package api

import "testing"

// The layout an rtorrent import leaves behind: the torrent's data sits
// directly in the category directory, so its content root IS that directory.
// Reported from the field as "0 OK, 1 failure" on a category change.
func TestLooseCategoryDirRecognisesAPayloadWithNoFolderOfItsOwn(t *testing.T) {
	cats := []category{
		{Name: "Done", SavePath: "/home/otiroblam/torrents/rtorrent/Done"},
		{Name: "Films-Moved", SavePath: "/home/otiroblam/torrents/Films"},
	}
	const loose = "/home/otiroblam/torrents/rtorrent/Done"
	if got := looseCategoryDir(loose, cats); got != loose {
		t.Fatalf("a content root that IS a category directory was not recognised as loose: %q", got)
	}
	if got := looseCategoryDir(loose+"/Some.Release.2019", cats); got != "" {
		t.Fatalf("a payload with a folder of its own was mistaken for a loose one: %q", got)
	}
	// A trailing slash in the config names the same directory.
	cats[0].SavePath = loose + "/"
	if got := looseCategoryDir(loose, cats); got != loose {
		t.Fatalf("a trailing slash in the category path defeated the check: %q", got)
	}
}

func TestPayloadRelPathsRefusesAnEmptyFileList(t *testing.T) {
	if _, err := payloadRelPaths(nil); err == nil {
		t.Fatal("an empty file list was accepted; the payload could not be told apart from the rest of the category")
	}
	rel, err := payloadRelPaths([]map[string]interface{}{{"path": "a.mkv"}, {"path": ""}, {"path": "b.mkv"}})
	if err != nil {
		t.Fatalf("payloadRelPaths: %v", err)
	}
	if len(rel) != 2 || rel[0] != "a.mkv" || rel[1] != "b.mkv" {
		t.Fatalf("unexpected file list: %v", rel)
	}
}
