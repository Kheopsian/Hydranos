package engine

// Outbound announce rate limiting.
//
// A hoard of 100k+ torrents announces in bursts: the scheduler pops every
// torrent whose deadline has passed and hands them to 512 workers at once.
// Behind a VPN that is a burst of new outbound flows through one tunnel, and
// some providers (Proton in particular) drop or throttle the tail of it — the
// announces then look like tracker failures. `announce_rate_limit` (per engine,
// announces/second, 0 = unlimited) puts a token bucket in front of every
// announce so the same volume goes out spread over time instead of in a wave.
//
// The bucket is shared per (engine, binding): with multi-tunnel bindings each
// tunnel gets its own budget, which is what the provider caps anyway.

import (
	"context"
	"errors"
	"log/slog"
	"sync"
	"time"
)

// announceWaitCap bounds how long one announce may sit waiting for a token.
// Past that the announce fails rather than pinning a worker: the scheduler
// re-plans it, which is the same back-pressure it applies to a tracker timeout.
const announceWaitCap = 120 * time.Second

// errAnnounceRateLimited is returned when the wait would exceed announceWaitCap,
// i.e. the configured rate cannot sustain the announce volume.
var errAnnounceRateLimited = errors.New("tracker announce: rate limit backlog, announce skipped")

type announceLimiter struct {
	scope string

	mu     sync.Mutex
	rate   float64 // tokens (announces) per second
	burst  float64 // bucket size, one second worth of credit
	tokens float64
	last   time.Time

	// Observability: how often the limiter actually bites, logged at most once
	// a minute so a mis-sized rate is visible without flooding the log.
	delayed int64
	dropped int64
	waitSum time.Duration
	lastLog time.Time
}

var (
	announceLimitersMu sync.Mutex
	announceLimiters   = map[string]*announceLimiter{}
)

// announceLimiterFor returns the shared limiter for a scope key, creating it on
// first use. perSec <= 0 means "no limit" and yields a nil limiter, which every
// method below treats as a no-op. A changed rate is applied in place so the
// callers keep whatever pointer they cached.
func announceLimiterFor(key string, perSec float64) *announceLimiter {
	if perSec <= 0 {
		return nil
	}
	announceLimitersMu.Lock()
	defer announceLimitersMu.Unlock()
	l := announceLimiters[key]
	if l == nil {
		l = &announceLimiter{scope: key, rate: perSec, burst: burstFor(perSec), tokens: burstFor(perSec)}
		announceLimiters[key] = l
		slog.Info("tracker announce: rate limit active", "scope", key, "per_sec", perSec)
		return l
	}
	l.mu.Lock()
	if l.rate != perSec {
		l.rate, l.burst = perSec, burstFor(perSec)
		if l.tokens > l.burst {
			l.tokens = l.burst
		}
	}
	l.mu.Unlock()
	return l
}

// burstFor sizes the bucket at one second of credit, never below a single
// token (a rate under 1/s must still let one announce through).
func burstFor(perSec float64) float64 {
	if perSec < 1 {
		return 1
	}
	return perSec
}

// wait blocks until this announce may go out. Nil receiver = no limit.
func (l *announceLimiter) wait(ctx context.Context) error {
	if l == nil {
		return nil
	}
	start := time.Now()
	deadline := start.Add(announceWaitCap)
	for {
		l.mu.Lock()
		now := time.Now()
		if l.last.IsZero() {
			l.last = now
		}
		l.tokens += now.Sub(l.last).Seconds() * l.rate
		l.last = now
		if l.tokens > l.burst {
			l.tokens = l.burst
		}
		if l.tokens >= 1 {
			l.tokens--
			waited := now.Sub(start)
			if waited > 0 {
				l.delayed++
				l.waitSum += waited
			}
			l.maybeLogLocked(now)
			l.mu.Unlock()
			return nil
		}
		need := time.Duration((1 - l.tokens) / l.rate * float64(time.Second))
		if need < time.Millisecond {
			need = time.Millisecond
		}
		if now.Add(need).After(deadline) {
			l.dropped++
			l.maybeLogLocked(now)
			l.mu.Unlock()
			return errAnnounceRateLimited
		}
		l.mu.Unlock()

		timer := time.NewTimer(need)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
	}
}

// maybeLogLocked emits a rolled-up throttling summary at most once a minute.
// Caller holds l.mu.
func (l *announceLimiter) maybeLogLocked(now time.Time) {
	if l.delayed == 0 && l.dropped == 0 {
		return
	}
	if !l.lastLog.IsZero() && now.Sub(l.lastLog) < time.Minute {
		return
	}
	avg := time.Duration(0)
	if l.delayed > 0 {
		avg = l.waitSum / time.Duration(l.delayed)
	}
	slog.Info("tracker announce: rate limited",
		"scope", l.scope, "per_sec", l.rate,
		"delayed", l.delayed, "avg_wait", avg.Round(time.Millisecond), "skipped", l.dropped)
	l.lastLog, l.delayed, l.dropped, l.waitSum = now, 0, 0, 0
}
