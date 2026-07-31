package logs

// Package logs is an in-process log hub: a bounded ring buffer that every log
// source (Go slog, Gin, the Rust engine's stdout/stderr) funnels into, plus a
// file mirror. The console is kept clean for a human startup banner; the ring
// buffer backs the UI "Logs" tab.

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// Entry is one normalized log line.
type Entry struct {
	TS     time.Time `json:"ts"`
	Source string    `json:"source"` // "go" | "gin" | "engine:race" | "engine:hoard"
	Level  string    `json:"level"`  // DEBUG | INFO | WARN | ERROR
	Msg    string    `json:"msg"`
}

// Hub is a concurrency-safe bounded ring buffer with live subscribers and an
// optional file mirror.
type Hub struct {
	mu     sync.RWMutex
	buf    []Entry
	max    int
	subs   map[chan Entry]struct{}
	mirror io.Writer
}

// Default is the process-wide hub.
var Default = NewHub(8000)

func NewHub(max int) *Hub {
	if max <= 0 {
		max = 5000
	}
	return &Hub{max: max, subs: make(map[chan Entry]struct{})}
}

// SetMirror tees every entry to w (best-effort).
func (h *Hub) SetMirror(w io.Writer) {
	h.mu.Lock()
	h.mirror = w
	h.mu.Unlock()
}

// SetMirrorFileBeside opens/creates <dir(configPath)>/name and mirrors to it.
func (h *Hub) SetMirrorFileBeside(configPath, name string) {
	f, err := os.OpenFile(filepath.Join(filepath.Dir(configPath), name),
		os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0644)
	if err != nil {
		return
	}
	h.SetMirror(f)
}

func (h *Hub) Add(e Entry) {
	if e.TS.IsZero() {
		e.TS = time.Now()
	}
	if e.Level == "" {
		e.Level = "INFO"
	}
	h.mu.Lock()
	h.buf = append(h.buf, e)
	if len(h.buf) > h.max {
		copy(h.buf, h.buf[len(h.buf)-h.max:])
		h.buf = h.buf[:h.max]
	}
	if h.mirror != nil {
		fmt.Fprintf(h.mirror, "%s %-13s %-5s %s\n",
			e.TS.Format("2006-01-02T15:04:05.000"), e.Source, e.Level, e.Msg)
	}
	for ch := range h.subs {
		select {
		case ch <- e:
		default:
		}
	}
	h.mu.Unlock()
}

// Query returns entries matching the filters (empty filter = match-all),
// capped to the newest limit entries.
func (h *Hub) Query(since time.Time, source, minLevel, q string, limit int) []Entry {
	h.mu.RLock()
	defer h.mu.RUnlock()
	lq := strings.ToLower(q)
	out := make([]Entry, 0, 256)
	for _, e := range h.buf {
		if !since.IsZero() && e.TS.Before(since) {
			continue
		}
		if source != "" && e.Source != source {
			continue
		}
		if minLevel != "" && levelRank(e.Level) < levelRank(minLevel) {
			continue
		}
		if q != "" && !strings.Contains(strings.ToLower(e.Msg), lq) {
			continue
		}
		out = append(out, e)
	}
	if limit > 0 && len(out) > limit {
		out = out[len(out)-limit:]
	}
	return out
}

// Subscribe returns a live channel and an unsubscribe func.
func (h *Hub) Subscribe() (<-chan Entry, func()) {
	ch := make(chan Entry, 256)
	h.mu.Lock()
	h.subs[ch] = struct{}{}
	h.mu.Unlock()
	return ch, func() {
		h.mu.Lock()
		if _, ok := h.subs[ch]; ok {
			delete(h.subs, ch)
			close(ch)
		}
		h.mu.Unlock()
	}
}

func levelRank(l string) int {
	switch strings.ToUpper(l) {
	case "DEBUG", "TRACE":
		return 0
	case "INFO":
		return 1
	case "WARN", "WARNING":
		return 2
	case "ERROR":
		return 3
	}
	return 1
}

// ---- slog handler ----------------------------------------------------------

type slogHandler struct {
	hub   *Hub
	level slog.Leveler
	attrs []slog.Attr
}

func NewSlogHandler(hub *Hub, level slog.Leveler) slog.Handler {
	return &slogHandler{hub: hub, level: level}
}

func (h *slogHandler) Enabled(_ context.Context, l slog.Level) bool {
	return l >= h.level.Level()
}

func (h *slogHandler) Handle(_ context.Context, r slog.Record) error {
	var b strings.Builder
	b.WriteString(r.Message)
	for _, a := range h.attrs {
		appendAttr(&b, a)
	}
	r.Attrs(func(a slog.Attr) bool {
		appendAttr(&b, a)
		return true
	})
	h.hub.Add(Entry{TS: r.Time, Source: "go", Level: r.Level.String(), Msg: b.String()})
	return nil
}

func (h *slogHandler) WithAttrs(as []slog.Attr) slog.Handler {
	nh := *h
	nh.attrs = append(append([]slog.Attr{}, h.attrs...), as...)
	return &nh
}

func (h *slogHandler) WithGroup(string) slog.Handler { return h }

func appendAttr(b *strings.Builder, a slog.Attr) {
	b.WriteByte(' ')
	b.WriteString(a.Key)
	b.WriteByte('=')
	b.WriteString(a.Value.String())
}

// ---- multi handler ---------------------------------------------------------

type multiHandler struct{ hs []slog.Handler }

func NewMultiHandler(hs ...slog.Handler) slog.Handler { return &multiHandler{hs: hs} }

func (m *multiHandler) Enabled(ctx context.Context, l slog.Level) bool {
	for _, h := range m.hs {
		if h.Enabled(ctx, l) {
			return true
		}
	}
	return false
}

func (m *multiHandler) Handle(ctx context.Context, r slog.Record) error {
	for _, h := range m.hs {
		if h.Enabled(ctx, r.Level) {
			_ = h.Handle(ctx, r.Clone())
		}
	}
	return nil
}

func (m *multiHandler) WithAttrs(as []slog.Attr) slog.Handler {
	nh := make([]slog.Handler, len(m.hs))
	for i, h := range m.hs {
		nh[i] = h.WithAttrs(as)
	}
	return &multiHandler{hs: nh}
}

func (m *multiHandler) WithGroup(name string) slog.Handler {
	nh := make([]slog.Handler, len(m.hs))
	for i, h := range m.hs {
		nh[i] = h.WithGroup(name)
	}
	return &multiHandler{hs: nh}
}

// ---- io.Writer sink (Gin) --------------------------------------------------

type writer struct {
	hub    *Hub
	source string
	level  string
}

func (h *Hub) NewWriter(source, level string) io.Writer {
	return &writer{hub: h, source: source, level: level}
}

func (w *writer) Write(p []byte) (int, error) {
	for _, line := range strings.Split(strings.TrimRight(string(p), "\n"), "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		w.hub.Add(Entry{Source: w.source, Level: w.level, Msg: line})
	}
	return len(p), nil
}

// ---- engine stream ingest --------------------------------------------------

// IngestStream scans r line-by-line into the hub, tagging each line with source
// and best-effort level parsed from the engine's tracing prefix. Returns on EOF.
func (h *Hub) IngestStream(source string, r io.Reader) {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for sc.Scan() {
		line := sc.Text()
		if strings.TrimSpace(line) == "" {
			continue
		}
		h.Add(Entry{Source: source, Level: parseEngineLevel(line), Msg: line})
	}
}

func parseEngineLevel(line string) string {
	for _, l := range []string{"ERROR", "WARN", "INFO", "DEBUG", "TRACE"} {
		if strings.Contains(line, " "+l+" ") {
			return l
		}
	}
	return "INFO"
}

// ---- credentials + startup banner ------------------------------------------

// WriteAdminCredentials writes the generated admin login to a 0600 file next to
// the config. The password is intentionally NEVER sent to the hub/mirror.
func WriteAdminCredentials(configPath, user, pass string) string {
	p := filepath.Join(filepath.Dir(configPath), "admin-credentials.txt")
	content := fmt.Sprintf("Hydra admin credentials (generated at first run)\nusername: %s\npassword: %s\n\nChange it via the UI or: hydra hash-password <newpass>\n", user, pass)
	if err := os.WriteFile(p, []byte(content), 0600); err != nil {
		return ""
	}
	return p
}

const hydraArt = `
                          ..
                     .-.. .+**++---..
                     .-*#####@@@@@#@#####*.
                        -*@@@@@@@@@@@@@@@#.
             .       .-#@@ o @ o @@@*. ..
         .  +*       .+#@@@@@@@@@@@@@*+--   ..
        *+.*@-..--.   *@@@@@@@#----+*@@@*   .#-  -
      .*@# o #@@@#*-..@@@@@@@@       .-..-+-- o -#*.
    .+#@@@@@@@@@@@@@@@@@@@@@@@.        -*@@@@@@@@#@#-
   .#@@@@@@@@@@@@@@@@@@@@@@@@@#.    .*@@@@@@@@@@@@@@@*.
  -#@@@@@@@@@##@@@@@@@@@@@@@@@@#-. +@@@@@@@@@@@@@@@@#@#-
-#@@@@#@@@@#.   .+@@@@@@@@@@@@@##*-*@@@@@@#+--#@@@@@@@@@*-.
*@@#-. #@#.       .@@@@@@@@@@@@@@#*+-*@@@+     -*@@*.-*@@@-
 --..-*@#          -@@@@@@@@@@@@@@#*+--#@.       -@@+..-+-.
     .++.           *@@@@@@@@##@@@@@***--.        +##-
          -+**####*++@#@@@@#@@@@@@@@@#*#-
        -#@@@@@@@@@@@###@@@@@@@@@@@@@####.      .....
      .*@@@@@@@@@@@@@###@@@@*+--#@@@@##@@-    .-+++-++.
      *@@@@@@@*----+####@@@##**+*@@@@*#@@-   .+*+*@@***.
     .@@@@#@@-      .@@#@@@@@@@@@@@@*#@@@--.      .@@##+
     -@@@@@#@.      -@@@#@@@@@@@@@#*@@@@#-@@*-....+@@@#--.
     .@@@@@@@#+...-.*@@@@@###@@###@@@@@@-@@@@@@@@@@@@#++*.
      +@@@@@@@@@@@#-@@@@@@@@@@@@@@@@@@#.+###@@@@@##***##.
       +@@@@@@@@@@-#@@@@-.-**##*@@@@@@#@@@@##**##@@@##+.
        .*@@@@@@@+#@@@@+       -@@@@@#**#####@@##***+-.
          .+*#@*-#@@@@+      .*@@@#+. ..-+*##**++++-.
              . ..--..       .....          ....
`

// PrintHeader prints the ASCII logo + version to the console at startup.
func PrintHeader(version string) {
	fmt.Fprint(os.Stdout, hydraArt)
	fmt.Fprintf(os.Stdout, "        H Y D R A   -   Typhon engine   -   v%s\n\n", version)
}

// PrintReady prints the "started" summary + (first run only) the credentials.
func PrintReady(host string, port int, user, pass string, isNew bool) {
	var b strings.Builder
	url := fmt.Sprintf("http://%s:%d", host, port)
	b.WriteString("  ------------------------------------------------------------\n")
	b.WriteString(fmt.Sprintf("   Engines started       API  %s\n", url))
	b.WriteString("  ------------------------------------------------------------\n\n")
	if isNew {
		lines := []string{
			"LOGIN  (first run - change it)",
			"user     : " + user,
			"password : " + pass,
			"saved in : admin-credentials.txt",
		}
		w := 0
		for _, l := range lines {
			if len(l) > w {
				w = len(l)
			}
		}
		bar := "   +" + strings.Repeat("-", w+2) + "+\n"
		b.WriteString(bar)
		for _, l := range lines {
			b.WriteString(fmt.Sprintf("   | %-*s |\n", w, l))
		}
		b.WriteString(bar)
	} else {
		b.WriteString("   Login: credentials already configured (admin-credentials.txt)\n")
	}
	b.WriteString("\n   Detailed logs -> hydra.log   |   UI \"Logs\" tab\n\n")
	fmt.Fprint(os.Stdout, b.String())
}
