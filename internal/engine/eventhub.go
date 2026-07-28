package engine

import (
	"sync"
	"sync/atomic"
)

// EventHub fan-outs serialized events from the Rust typhon push stream
// to in-process subscribers (HTTP SSE handlers).
//
// Each Subscribe() returns an int64 id + a buffered chan []byte. Publish()
// tries to send to every subscriber; a slow consumer whose buffer is full
// silently loses the event (acceptable for stats_snapshot — the next tick
// carries the fresh state, not a diff).
type EventHub struct {
	mu   sync.RWMutex
	subs map[int64]chan []byte
	next atomic.Int64
	cap  int
}

func NewEventHub(bufCap int) *EventHub {
	if bufCap <= 0 {
		bufCap = 64
	}
	return &EventHub{subs: make(map[int64]chan []byte), cap: bufCap}
}

// Subscribe registers a new consumer. Caller must invoke Unsubscribe(id) when
// done (a good candidate for defer when hooking into an HTTP request lifetime).
func (h *EventHub) Subscribe() (int64, <-chan []byte) {
	c := make(chan []byte, h.cap)
	id := h.next.Add(1)
	h.mu.Lock()
	h.subs[id] = c
	h.mu.Unlock()
	return id, c
}

func (h *EventHub) Unsubscribe(id int64) {
	h.mu.Lock()
	if ch, ok := h.subs[id]; ok {
		delete(h.subs, id)
		close(ch)
	}
	h.mu.Unlock()
}

// Publish sends data to all subscribers; drops on any full buffer.
func (h *EventHub) Publish(data []byte) {
	h.mu.RLock()
	for _, ch := range h.subs {
		select {
		case ch <- data:
		default:
			// drop on slow consumer
		}
	}
	h.mu.RUnlock()
}

func (h *EventHub) NumSubs() int {
	h.mu.RLock()
	defer h.mu.RUnlock()
	return len(h.subs)
}
