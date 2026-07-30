//go:build windows

package main

// defaultEngineTCP selects the TCP loopback IPC by default on platforms
// without Unix domain sockets in this path (Windows). Linux defaults to unix.
const defaultEngineTCP = true

// defaultAPIHost: a desktop install should not expose its UI to the LAN by
// default. Users who want LAN access set api_host = "0.0.0.0" in the config.
const defaultAPIHost = "127.0.0.1"

// raiseNofileLimit is a no-op on Windows: there is no RLIMIT_NOFILE; socket
// handle limits are governed by the OS and are effectively unbounded here.
func raiseNofileLimit(target uint64) {}
