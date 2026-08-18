package logs

import (
	"os"
	"strconv"
	"sync"
)

// Bounds for the hydra.log mirror.
//
// Every hub entry is mirrored to disk, and the engines emit one line per
// inbound peer connection. On a seedbox taking north of a million connections
// an hour that is ~99% of the file and gigabytes a day, against a file that
// nothing ever truncated: the prod instance reached 41 GB on the cache SSD
// before anyone looked. Rotation lives here rather than in logrotate because
// Hydra owns the file and ships to people who have no logrotate at all.
const (
	defaultMaxLogBytes int64 = 128 << 20 // 128 MiB per generation
	defaultMaxLogFiles       = 5         // hydra.log + .1 .. .4 => 640 MiB ceiling
)

// rotatingFile is an io.Writer that keeps path under maxBytes by shifting the
// generations down (path -> path.1 -> path.2 ...) and starting a fresh file.
// The oldest generation is deleted, so disk usage is bounded by
// maxBytes * maxFiles no matter how long the daemon runs.
type rotatingFile struct {
	mu       sync.Mutex
	path     string
	maxBytes int64
	maxFiles int
	f        *os.File
	size     int64
}

func newRotatingFile(path string, maxBytes int64, maxFiles int) (*rotatingFile, error) {
	if maxBytes <= 0 {
		maxBytes = defaultMaxLogBytes
	}
	if maxFiles < 1 {
		maxFiles = 1
	}
	r := &rotatingFile{path: path, maxBytes: maxBytes, maxFiles: maxFiles}
	if err := r.open(); err != nil {
		return nil, err
	}
	return r, nil
}

func (r *rotatingFile) open() error {
	f, err := os.OpenFile(r.path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
	if err != nil {
		return err
	}
	var size int64
	if st, statErr := f.Stat(); statErr == nil {
		size = st.Size()
	}
	r.f, r.size = f, size
	return nil
}

// rotate shifts the generations down and reopens a fresh file. Errors are
// deliberately swallowed: losing a log line must never take the daemon down,
// and a rotation that fails is retried on the next write.
func (r *rotatingFile) rotate() {
	if r.f != nil {
		_ = r.f.Close()
		r.f = nil
	}
	if archives := r.maxFiles - 1; archives >= 1 {
		// Drop the oldest first so no generation is overwritten before it moves.
		_ = os.Remove(r.path + "." + strconv.Itoa(archives))
		for i := archives - 1; i >= 1; i-- {
			_ = os.Rename(r.path+"."+strconv.Itoa(i), r.path+"."+strconv.Itoa(i+1))
		}
		_ = os.Rename(r.path, r.path+".1")
	} else {
		_ = os.Remove(r.path)
	}
	_ = r.open()
}

// Write never reports an error: the caller is the log hub, which holds its lock
// while mirroring and has nothing useful to do with a disk failure.
func (r *rotatingFile) Write(p []byte) (int, error) {
	r.mu.Lock()
	defer r.mu.Unlock()

	if r.f == nil {
		if err := r.open(); err != nil {
			return len(p), nil
		}
	}
	// Rotate before writing so a single line never straddles two generations.
	if r.size > 0 && r.size+int64(len(p)) > r.maxBytes {
		r.rotate()
		if r.f == nil {
			return len(p), nil
		}
	}
	n, err := r.f.Write(p)
	r.size += int64(n)
	if err != nil {
		// Drop the handle so the next write reopens rather than spinning on a
		// dead descriptor (the file may have been removed underneath us).
		_ = r.f.Close()
		r.f = nil
	}
	return len(p), nil
}
