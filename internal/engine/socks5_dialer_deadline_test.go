package engine

import (
	"context"
	"net"
	"testing"
	"time"
)

// A SOCKS5 proxy that completes the TCP connect and then says nothing is the
// exact shape that pinned three dials for 29 h in production: every read after
// the connect was deadline-free, so the dial never returned and net/http never
// reclaimed the wantConn queued behind it.
func TestDialSOCKS5hGivesUpOnSilentProxy(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	accepted := make(chan net.Conn, 1)
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		accepted <- c // held open, never written to
	}()

	// The caller's deadline is the shorter one, so the dial must honour it
	// rather than sitting on socks5HandshakeTimeout.
	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()

	done := make(chan error, 1)
	go func() {
		_, err := dialSOCKS5h(ctx, &net.Dialer{}, ln.Addr().String(), "", "", "tracker.example", 80)
		done <- err
	}()

	select {
	case err := <-done:
		if err == nil {
			t.Fatal("dialSOCKS5h succeeded against a proxy that never replied")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("dialSOCKS5h blocked past the caller's deadline: the handshake is unbounded again")
	}
	select {
	case c := <-accepted:
		c.Close()
	default:
	}
}

// The data phase must not inherit the handshake deadline.
func TestDialSOCKS5hClearsDeadlineOnSuccess(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		defer c.Close()
		buf := make([]byte, 3)
		if _, err := readFullTest(c, buf); err != nil {
			return
		}
		c.Write([]byte{5, 0}) // no auth
		req := make([]byte, 5)
		if _, err := readFullTest(c, req); err != nil {
			return
		}
		rest := make([]byte, int(req[4])+2)
		if _, err := readFullTest(c, rest); err != nil {
			return
		}
		// CONNECT reply, ATYP=1 (IPv4) + BND.ADDR + BND.PORT
		c.Write([]byte{5, 0, 0, 1, 0, 0, 0, 0, 0, 0})
		time.Sleep(400 * time.Millisecond) // past any handshake deadline
		c.Write([]byte("payload"))
		time.Sleep(200 * time.Millisecond)
	}()

	conn, err := dialSOCKS5h(context.Background(), &net.Dialer{}, ln.Addr().String(), "", "", "tracker.example", 80)
	if err != nil {
		t.Fatalf("handshake failed: %v", err)
	}
	defer conn.Close()

	buf := make([]byte, 7)
	if _, err := readFullTest(conn, buf); err != nil {
		t.Fatalf("read after handshake failed -- the handshake deadline leaked into the data phase: %v", err)
	}
	if string(buf) != "payload" {
		t.Fatalf("got %q", buf)
	}
}

func readFullTest(c net.Conn, b []byte) (int, error) {
	n := 0
	for n < len(b) {
		m, err := c.Read(b[n:])
		n += m
		if err != nil {
			return n, err
		}
	}
	return n, nil
}
