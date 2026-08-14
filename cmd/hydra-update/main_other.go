//go:build !windows

// hydra-update is a Windows-only helper: it exists to replace hydra.exe while
// nothing holds a lock on it. Elsewhere the packaging handles upgrades (a
// container image, a distribution package), so there is nothing for it to do.
package main

import (
	"fmt"
	"os"
)

func main() {
	fmt.Fprintln(os.Stderr, "hydra-update is only used on Windows; update Hydra through your package manager or container image.")
	os.Exit(1)
}
