package ltclient

import (
	"bufio"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// TestEventsStreamWithoutConcurrentCall verifies that pushed events arrive
// in real time even when no RPC call is in flight. This is the exact bug
// that caused SSE batching: before the refactor, the scanner was only read
// from inside call(), so events accumulated in the socket buffer between
// calls and were dispatched in a burst when the next call happened.
func TestEventsStreamWithoutConcurrentCall(t *testing.T) {
	sockPath := filepath.Join(t.TempDir(), "t.sock")

	// Fake engine: replies to subscribe_events, then emits 10 events
	// spaced ~100ms apart without any request-reply activity in between.
	ln, err := net.Listen("unix", sockPath)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer ln.Close()

	const nEvents = 10
	const gap = 100 * time.Millisecond

	serverDone := make(chan struct{})
	go func() {
		defer close(serverDone)
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()

		// Read the subscribe_events request line, send a matching reply.
		reader := bufio.NewReader(conn)
		line, err := reader.ReadBytes('\n')
		if err != nil {
			t.Errorf("server read: %v", err)
			return
		}
		var req Request
		if err := json.Unmarshal(line, &req); err != nil {
			t.Errorf("server parse req: %v", err)
			return
		}
		if req.Method != "subscribe_events" {
			t.Errorf("unexpected method: %s", req.Method)
			return
		}
		reply := []byte(`{"id":` + itoa(req.ID) + `,"result":{"ok":true}}` + "\n")
		if _, err := conn.Write(reply); err != nil {
			return
		}

		// Emit events spaced by `gap`, no further request-reply activity.
		for i := 0; i < nEvents; i++ {
			time.Sleep(gap)
			frame := []byte(`{"event":"stats_snapshot","data":{"seq":` + itoa(int64(i)) + `}}` + "\n")
			if _, err := conn.Write(frame); err != nil {
				return
			}
		}
		// Hold the connection open briefly so the client drains its buffer.
		time.Sleep(200 * time.Millisecond)
	}()

	c, err := Connect(sockPath)
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	defer c.Close()

	var mu sync.Mutex
	var arrivals []time.Time
	c.SetEventHandler(func(ev Event) {
		mu.Lock()
		arrivals = append(arrivals, time.Now())
		mu.Unlock()
	})

	if err := c.SubscribeEvents(); err != nil {
		t.Fatalf("subscribe: %v", err)
	}

	// Wait just a bit more than the total server emission window.
	deadline := time.After(gap*time.Duration(nEvents) + 500*time.Millisecond)
	<-deadline

	mu.Lock()
	got := append([]time.Time(nil), arrivals...)
	mu.Unlock()

	if len(got) != nEvents {
		t.Fatalf("expected %d events, got %d", nEvents, len(got))
	}

	// Events must arrive spread out, not batched. If the old bug were still
	// there, all events would land inside a tiny window at the end (or never,
	// since no call() runs). Require: >=7 inter-arrival gaps above gap/2.
	spread := 0
	for i := 1; i < len(got); i++ {
		if got[i].Sub(got[i-1]) > gap/2 {
			spread++
		}
	}
	if spread < 7 {
		t.Fatalf("events appear batched (only %d/9 inter-arrival gaps > %v)", spread, gap/2)
	}

	<-serverDone
}

// itoa avoids pulling strconv just for a tiny helper.
func itoa(n int64) string {
	if n == 0 {
		return "0"
	}
	buf := make([]byte, 0, 20)
	neg := n < 0
	if neg {
		n = -n
	}
	for n > 0 {
		buf = append([]byte{byte('0' + n%10)}, buf...)
		n /= 10
	}
	if neg {
		buf = append([]byte{'-'}, buf...)
	}
	return string(buf)
}

// silence unused import warning for os in case t.TempDir evolves
var _ = os.Getenv
