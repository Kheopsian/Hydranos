package engine

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// removeTorrentFiles deletes exactly the files a torrent owns under savePath,
// then prunes the directories its removal emptied.
//
// It never calls os.RemoveAll on savePath/name. A race save_path is flat and
// shared: a multi-file torrent whose info.name happens to match a directory
// holding another torrent's data would take that data down with it, and the
// callers are not all human — drain deletes with deleteFiles=true in a loop to
// reclaim space. The engine already deletes file-by-file for this exact reason
// (typhon-engine remove_torrent); this is the Go side finishing the job for the
// files the engine did not know about, on the same terms.
//
// files carries BEP-3 relative paths, as get_files returns them: relative to
// the info.name directory for a multi-file torrent, equal to info.name for a
// single-file one.
//
// An empty file list deletes nothing and is not an error: without the list
// there is no way to tell what the torrent owns, and guessing is what this
// function exists to stop. The caller logs it.
func removeTorrentFiles(savePath, name string, files []ltclient.FileInfo) (removed int, err error) {
	if savePath == "" || len(files) == 0 {
		return 0, nil
	}

	root := savePath
	if !isSingleFileLayout(name, files) {
		root = filepath.Join(savePath, name)
	}

	var firstErr error
	for _, f := range files {
		abs, ok := resolveOwnedPath(root, f.Path)
		if !ok {
			// A .torrent is attacker-controlled input: a "../.." component
			// would put this delete outside the save path entirely.
			if firstErr == nil {
				firstErr = fmt.Errorf("refusing to delete %q: escapes %q", f.Path, root)
			}
			continue
		}
		switch err := os.Remove(abs); {
		case err == nil:
			removed++
		case os.IsNotExist(err):
			// Already gone: the engine deletes the same list first.
		default:
			if firstErr == nil {
				firstErr = err
			}
		}
	}

	if root != savePath {
		pruneEmptyDirs(root)
	}
	return removed, firstErr
}

// isSingleFileLayout reports whether the torrent stores its payload directly in
// save_path rather than under an info.name directory. A single-file torrent has
// exactly one file, whose BEP-3 path is info.name itself.
func isSingleFileLayout(name string, files []ltclient.FileInfo) bool {
	return len(files) == 1 && files[0].Path == name
}

// resolveOwnedPath joins a torrent-supplied relative path onto root and refuses
// anything that does not stay under it.
func resolveOwnedPath(root, rel string) (string, bool) {
	if rel == "" || filepath.IsAbs(rel) {
		return "", false
	}
	abs := filepath.Join(root, rel)
	if abs != root && !strings.HasPrefix(abs, root+string(os.PathSeparator)) {
		return "", false
	}
	return abs, true
}

// pruneEmptyDirs removes root and the directories below it that are empty,
// deepest first. os.Remove only succeeds on an empty directory, so a foreign
// file anywhere in the tree keeps its parents alive.
func pruneEmptyDirs(root string) {
	var dirs []string
	_ = filepath.WalkDir(root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			dirs = append(dirs, path)
		}
		return nil
	})
	// Deepest first, so a directory is visited after the subdirectories that
	// might have been emptying it.
	sort.Slice(dirs, func(i, j int) bool { return len(dirs[i]) > len(dirs[j]) })
	for _, d := range dirs {
		_ = os.Remove(d)
	}
}
