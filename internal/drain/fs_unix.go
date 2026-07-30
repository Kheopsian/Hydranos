//go:build !windows

package drain

import "syscall"

// fsTotalFree returns total and available bytes of the filesystem at path.
func fsTotalFree(path string) (total, free int64, err error) {
	var st syscall.Statfs_t
	if err = syscall.Statfs(path, &st); err != nil {
		return 0, 0, err
	}
	return int64(st.Blocks) * st.Bsize, int64(st.Bavail) * st.Bsize, nil
}
