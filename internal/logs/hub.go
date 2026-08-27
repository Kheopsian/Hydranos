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
	"sync/atomic"
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
	// mirrorName describes the mirror destination for the startup banner.
	mirrorName string
}

// Default is the process-wide hub.
var Default = NewHub(8000)

func NewHub(max int) *Hub {
	if max <= 0 {
		max = 5000
	}
	return &Hub{max: max, subs: make(map[chan Entry]struct{})}
}

// StdoutMirrorEnv asks for the mirror on stdout instead of the log file.
// Set it to any value other than the usual "off" spellings (0/false/no/off).
const StdoutMirrorEnv = "HYDRA_LOG_STDOUT"

// StdoutMirror reports whether the environment asks for the log mirror on
// stdout. A file inside the config volume is the wrong place to look when
// Hydra runs under Docker, systemd or a process supervisor: those want the
// stream on stdout, where `docker logs` and the journal pick it up.
func StdoutMirror() bool {
	switch strings.ToLower(strings.TrimSpace(os.Getenv(StdoutMirrorEnv))) {
	case "", "0", "false", "no", "off":
		return false
	}
	return true
}

// SetMirror tees every entry to w (best-effort). name describes the
// destination for the startup banner; it may be empty.
func (h *Hub) SetMirror(w io.Writer, name string) {
	h.mu.Lock()
	h.mirror, h.mirrorName = w, name
	h.mu.Unlock()
}

// SetMirrorFileBeside opens/creates <dir(configPath)>/name and mirrors to it,
// rotating the file so it cannot grow without bound (see rotate.go). A mirror
// that cannot be opened is not fatal: the hub keeps serving the UI and the
// console, it just has nowhere to write.
func (h *Hub) SetMirrorFileBeside(configPath, name string) {
	f, err := newRotatingFile(filepath.Join(filepath.Dir(configPath), name),
		defaultMaxLogBytes, defaultMaxLogFiles)
	if err != nil {
		return
	}
	h.SetMirror(f, name)
}

// SetMirrorStdout mirrors every entry to stdout instead of a file. Unlike the
// file mirror this needs no config path, so it can be attached before the
// config is resolved and catch the very first lines of the run.
//
// The write goes through an async queue, which the file mirror does not need.
// Add formats the mirror line while holding the hub lock, so a mirror that
// blocks blocks every log producer in the process with it -- the engines'
// stdout ingestion, gin, every slog call. A regular file write returns; stdout
// under a supervisor is a pipe, and a pipe whose reader stalls does not.
func (h *Hub) SetMirrorStdout() { h.SetMirror(newAsyncWriter(os.Stdout, asyncMirrorDepth), "stdout") }

// asyncMirrorDepth is the queue the stdout mirror absorbs a stalled reader
// with. At the production rate -- the engines log a line per inbound peer
// connection -- this is a few seconds of slack before anything is dropped.
const asyncMirrorDepth = 4096

// asyncWriter hands writes to a single drain goroutine through a bounded
// queue, and drops rather than blocks when the queue is full: losing log lines
// beats wedging the process that produces them. Drops are never silent -- the
// count is reported inline as soon as the consumer catches up.
type asyncWriter struct {
	ch      chan []byte
	dropped atomic.Uint64
}

func newAsyncWriter(w io.Writer, depth int) *asyncWriter {
	a := &asyncWriter{ch: make(chan []byte, depth)}
	go func() {
		for b := range a.ch {
			if n := a.dropped.Swap(0); n > 0 {
				fmt.Fprintf(w, "%s %-13s %-5s log mirror dropped %d line(s): consumer too slow\n",
					time.Now().Format("2006-01-02T15:04:05.000"), "logs", "WARN", n)
			}
			_, _ = w.Write(b)
		}
	}()
	return a
}

// Write never blocks and never fails: a log mirror has nothing useful to do
// with a write error, exactly as the file mirror treats its own.
func (a *asyncWriter) Write(p []byte) (int, error) {
	b := make([]byte, len(p))
	copy(b, p)
	select {
	case a.ch <- b:
	default:
		a.dropped.Add(1)
	}
	return len(p), nil
}

// MirrorName describes where the mirror writes ("hydra.log", "stdout"), or ""
// when there is no mirror.
func (h *Hub) MirrorName() string {
	h.mu.RLock()
	defer h.mu.RUnlock()
	return h.mirrorName
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

// LevelAtLeast reports whether level is >= min in severity.
func LevelAtLeast(level, min string) bool { return levelRank(level) >= levelRank(min) }

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

// parseEngineLevel extracts the level from a raw engine line.
//
// The engine writes ANSI-coloured tracing output, so the level arrives wrapped
// in escape codes ("\x1b[33m WARN\x1b[0m") rather than padded with spaces.
// Matching on " "+level+" " therefore never hit on a real line and every engine
// entry was filed as INFO -- warnings and errors included, which made the Logs
// tab level filter useless for engine sources. Match on a word boundary
// instead, so an INFO line that merely mentions ERRORS is still INFO.
func parseEngineLevel(line string) string {
	for _, l := range []string{"ERROR", "WARN", "INFO", "DEBUG", "TRACE"} {
		if i := strings.Index(line, l); i >= 0 && isDelimitedWord(line, i, len(l)) {
			return l
		}
	}
	return "INFO"
}

func isDelimitedWord(s string, i, n int) bool {
	// The byte that terminates an ANSI escape sequence is itself a letter
	// ("\x1b[31m"), so a bare letter check would reject "\x1b[31mERROR" -- the
	// exact shape the engine emits when it does not pad the level with a space.
	if i > 0 && isASCIILetter(s[i-1]) && !endsANSI(s, i) {
		return false
	}
	if end := i + n; end < len(s) && isASCIILetter(s[end]) {
		return false
	}
	return true
}

// endsANSI reports whether s[:i] ends with a CSI escape sequence: ESC, "[",
// digits and semicolons, then a final letter.
func endsANSI(s string, i int) bool {
	j := i - 1
	if j < 0 || !isASCIILetter(s[j]) {
		return false
	}
	for j--; j >= 0 && (s[j] == ';' || (s[j] >= '0' && s[j] <= '9')); j-- {
	}
	return j >= 1 && s[j] == '[' && s[j-1] == 0x1b
}

func isASCIILetter(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
}

// ---- startup banner ---------------------------------------------------------
//
// There is deliberately no admin-credentials.txt any more. It wrote a plaintext
// admin password next to the config, where it lived forever; and being written
// at the very end of boot, it was missing in exactly the case that needed it —
// a boot that failed after the password had been generated. Hydra no longer
// generates a password at all: the UI asks for one on first run.

const hydraArt = `
                          ..
                     .-.. .+**++---..
                     .-*#####@@@<->@#####*.
                        -*@@@@@@@@@@@@@@@#.
             .       .-#@@@@@@@@@@@@*. ..
         .  +*       .+#@@@@@@@@@@@@@*+--   ..
        *+.*@-..--.   *@@@@@@@#----+*@@@*   .#-  -
      .*@#@@@#@@@#*-..@@@@@@@@       .-..-+--*@*-#*.
    .+#@@@@@@@@@@@@@@@@@@@@@@@.        -*@@@@@@@@#@#-
   .#@<->@@@@@@@@@@@@@@@@@@@@@#.    .*@@@@@@@@@@@@@@@*.
  -#@@@@@@@@@##@@@@@@@@@@@@@@@@#-. +@@@@@@@@@@@@@@@<->#-
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

const hydraArt80 = `
                                    +##+++
                             +##+-.  -+++++-------...---
                               -+###+++++++##+-+++++++++-
                                  +######+++++++++++###++
                               +#####++++++++++++#+  ++--
                  -           #####-+++##+++++###
                 +#          +++##-++#####+++++++#+-+ -     -
            #  ++#+           +##-++######++++#########     +-
           ## -+## +++++++   +##+++###+++         +###+     ++-  .+
         .##-++#########+.   ###++###++++                    +++- ++
        +#+++####+++++++####+###++###++++            -++#######++--+++
      -++++++#++++++++++++-+#########++++           +###++---++##++---+-
     ++####++++##########+++++###+####++++       -###++++++++++++++++++---
    -+#+-#####++######++###+++++#######-++--    ################++++++-+--.
   ++#########+##+  -++++++##++++#######+----  ########++++++###++++++++++--
+++######+######+      +++++###+++#######+-----+#####++++++  -##+++###+++++---
+#####+   ####           +++++##++#########-----.+##++++-      +#####- +##++++-
 +##++-  .##+             ++++##############+------++++-         +###+   +###+
       -+###              +++++#########+####++------++           +###+    +
        +#+#               +++++#######--+####++------+           +####+
                  -------  -++-+####++#+-##+###++-----.-
              -+++--+++##++-+---###+++#+-##+####++-----
            +++++++#########+---####+++++++++++#+------
          +++++-+###########+---###+++-----+++###-------       .-------.
         -++++++############+---+##++---...-++#+#+---++-       -+++------.
        -+++++-#####++  +++#+---+##+--------+++##----++-     +++++++++----.
        ##++++-###+         ----+###++++++--++##+---+++-     -     -+++----
       ####+++-+##          ++---####+++++++##+#---++++-+-          +++----.
       ####++++-+#         .++++--+#####+##+##+--++++++++++--      -+++------
        ####++++--+        ++++++----+#####+---+++#+++++####++++++++#++++---.
        #####++++++---++++.+++++#+++++-----+++++##++++++++++#######+++++----
         ######+++++++++++++++#########+++++#####+++--+++++++++++++++----++-
          ########+++++++ +++#### +##############+++++++++---++++++++++++-
           #############+++++####       +  #####++++++++++++++--+++++++---
            -##########++######+          ########++-+++++-+++++++---+++-
              +#######-+#######          +######+  -+++++++++-++++++++
                  +#+++######+         -###+++            +++++++-
`

const hydraArt100 = `
                                            ++
                                    -       +####+++-......
                                    +++++-.   +####+++-------....----.
                                      ####++++++++++++++++++++----++--.
                                        +########++++++++--+++++++++++.
                                         +######+++++++++++++++#####++
                                      +#####+++++++++++++++#+#+  .#+--
                     --              ######+++++###+++++####+
                    -+#            +##++#+++++######+++++++###+ -
              +    ++##            +  ##+-+++##############++####+++      +-
             +#+  -+##+              ###++++###+###+-++-+#######+##+      ++-    --
            +##+--+### +++###+++-   ####+++###+++++          -+###+       +++-.  ++-
           +##+-++###########+-     ####++###+++++            ++           +++---++++
          +#+++++######++++#######++####+####+++++               -+++#######+#++--++++
        +++++++###++++++++++---++#######++###+++++                ++############++---++-
      +-+++##++#++++++##+++++++++++#####+#####+++++            ####++++++++++++++++++-----
     -++#####+++++#############++++++#########++++++         +###+++++++++++++++++++++++----
     ++##--######++#######+++####++++++########+-+++--     +###################++++++++++---.
   -++###########++######++#++++###+++++#########------   +#############++#####-+++++++-+++--.
 +++############++##+      -++++++###++++#########+------ #########++++++++++##++++#++++++++---.
++########++######+          ++++++###++++#########+------.+#####++++++     +###+++#####+++++----.
+######    ######             ++++++###++############+-------+##+++++-         +#####+  +###++++--
+###+#+    +###+               -+++++#################+-------++++++-            ####++   +####+-
   ++    +####+                 ++++++#################++------.-+++              +###++  -+-+#+
        +#####+                 -+++++##########+-+######+---------+               ####++-
          ++                     ++++-+########+---+######++------.-               +####+
                     .--------   -+++-+#####++###-+#+++###+++------..
                 -++++---++++++++------#####+-+##-+##++####++-------
               ++++++++###########+----+####+++++-++++##++##+-------
             ++++++-+#############+----+##+#+++++--+++++++##+--------           -----..
           +++++++-+##############+----+###+++----..-++++#+#+--------         .----------.
          -++++++--###############+----+###++-----...-+++#+++----+++-         -+++++-------.
          +++++++-+######+-  -++##+----+###++----------+++#++----+++-       ++++++++++------.
         ++#+++++-#####          ------+####+-++------+++###+----+++-      -+-     ++++------
         +##+++++-+###            -+----+####++++++-++++#+#+----+++++               ++++-----
         ####++++--###            +++----+#####++++++++####---++++++ -++--          ++++-------
         ####+++++--##           .++++++--+######+###+###+.-++++++++ +++++--.      -++++--------
         #####++++++-++          +++++++----++#######++---+++++++++ +####++++++---++##++++-----.
         ######+++++++---.    -+ +++++++++++-----------+++++##+++++-#########+++++##++++++-----
          #######+++++++++++++++-++++#######++++++++++++++###+++++-+++++++++++++++++++++----+--
          +#######+++++++++++++-++++#########################++++ ++-+++++++++++++-------+++-
           +##########++++++++++++++####+ ++################++++++++++++++-++++++++++++++++-
            #################++++++#####        --+-.######+++++++++++++++++---++++++++++.-
              +#############+-+#++#####-            ##########+++++--++++++++++--+-----++--
                +##########+ +########+            +########++++++++#+++++++++++++++++++
                  -#######+-+########+            +#######++        -++++++++-.-++++-
                      +#+ -++++####++           -+###+++                ++++++++-.
`

const hydraArt120 = `
                                                     -+
                                           .          .####-.
                                            ##-+        -#######++------..       ..
                                             -###+++++----------++++++++------------.
                                                -######+++++++++####-.-+##+++++++---.
                                                    -+########+++++++++++++++++#####.
                                                 -+######++++++++++++++++++##+++#+-#
                                               +######--++++++++++++++###+-    .+ ..
                          .-                 +######+-++++####++++++####.
                          ++                ###+###--+++######+-+++++++###
                         +#+               #- -###-++++#########+++++++++##+ -+ --       -
                  #     ++#-                 +###-++++####################+####-##       ++.
                 ##   -++#+                 .###+-+++###++###+          ++###++##+       +++-     +.
                ##+ .+++##   .-+#####+-.    ####-+++###+++++-              -###+          #++-    ++
              .###.--++##-#########.        ####++++###+++++                              .+++--  +++
             -##+-+++##################+   +####++++###+++++                   -#++####+++- ++++--.+++-
            ##--++++####+-----------+#####+#####++++###+++++                      -+###########+++---+++.
          ++--+++++##++++++++++++++++---+#######++#####+++++-                  -+######++++######++++--++-
        ++++####+++++++++#######+++++++++-+######+######-++++-              +####++++++++++++++++#++++------
       -++######++++++###############++++++--####+######+-++++-           +###+++++++++++++++++++++++++#+----.
      .++##-.+######+++########++++#####+++++++##########+-++++-        +###+###################+++++++#+++---.
     .++##++#########++############+++####++++++##########+--+---.     +###############++++####++++++++++.-#---.
    ++###############+###.      -+#++++++###+++++###########-------   ############++++++#######-++++#++++++##+---
 -++###############++##+           +++++++###+++++###########+------- -#########++++++-     -###-+++###++++++++---..
-+#########-  .######+              -++++++###++++#############-------. -######+++++.        -###++++##+####+++++++--.
-+#####+      +#####                 .++++++###++++#############+-------. +###+++++.            +###+##     +###++++-.
 .###.+#-     +###-                   .++++++####################++-------. #++++++               ###+#-      #####+-
   .++      #+###+                     -++++-#####################++-------- -++++.                +###+     +. +#+
          +-.####                       ++++++#############++#######++-------  +++                  #####+
           +####+.                      -++++-############+---#######++-------. -+                  +######.
                                        .++++-+##########++--+########+++------. -.                  .-+++
                           ...---...     -++---#######-+###--##+-+#####+++------.
                      --++-------++++++-.-+----######---+##--###+######++++------.
                   -+++++++-+#############-----+######-+#++--++#+###++##+---------
                 -++++++--+###############-----+##++##+++++-++++++++++###---------.
                +++++++--################+-----+#####++++-----++++++++###----------            ..-------..
              .+#+++++--#################+------####+++-----....-++#+++##+---------                 ..-----.
             .+++++++--###################------####++-------...--+++##+#+-----+++-           .++++++-..-----.
             ++#+++++-+#######-     .-+###------####++-+----------++++#+#+-----+++-         -+++++++++++.-----.
            -###+++++-+#####             +------#####+-++---------+++####-----++++-        -+-     .+++++------.
            ####+++++-+###+              .+-----#####++-+++++++--+++####+-----+++++        -         -++++-----.
           #####+++++--###-              -+++----######+++----+++#+##-##.----+++++- -                 +++++----. .
           +####+++++--+##               -+++-----########++++++++#+##+.-+++++++++ -+++-.            .++++-----..-
           +#####+++++--##-              -++++++-+-+#######++########-.-++++++++++ -++++++.         .-++++----- .-.
           +#####+++++++--#-             +++++++++----+###########+..-+++++#+++++ -#####++++++----+++###++++++..--
            ######++++++++--+.          -++++++-+++------------...-++++++##+++++- +#########+++++++###++++++- .---
            -#######+++++++++---...-++- ++++++##+++++++++++++++++++++++###+++++- ++++++++++########+++++++-  --+-.
             ########+++++++++++++++++..+++++#########++++++++++++++#####+++++. +++++++++++++++++++++++.  .-++++.
              #########++++++++++++++- +++++#####+#######################++++.         .-+++++++++++++-+---+++-.
              -###########+++++++++++ -++++#####-   -+##################++++.+++++++++++-  -+++++++++++++++++-
                ####################  +++++#####                -######++++++++++++++++++++-.  .++++++++++.  .
                 +#################  +++++#####-                ##############++++-++++++++++++--.      .--+-
                   +############## -####+######                ############+. .++#+++.  .+#+++++++++++-+++-
                     -##########+ -##########+               .##########+   ++--+++++##++-  .-+++++++++.
                        -######- -###########               -########+.              ++++++++-
                                ---+++++++--              -++-----                       ---------
`

// bannerLayout picks the logo for the current terminal plus its display width.
// art is empty when the terminal is too narrow for any logo (wordmark only).
func bannerLayout() (art string, width int) {
	switch w := termWidth(); {
	case w == 0:
		// Detached / non-TTY (docker logs, web viewers): a readable mid logo.
		return hydraArt80, 79
	case w >= 118:
		return hydraArt120, 118
	case w >= 98:
		return hydraArt100, 98
	case w >= 79:
		return hydraArt80, 79
	case w >= 60:
		return hydraArt, 58
	default:
		return "", 46
	}
}

// centered left-pads s to sit centered within a width-column field.
func centered(s string, width int) string {
	if pad := (width - len(s)) / 2; pad > 0 {
		return strings.Repeat(" ", pad) + s
	}
	return s
}

// PrintHeader prints the logo + version line, sized to the terminal.
func PrintHeader(version string) {
	art, w := bannerLayout()
	if art != "" {
		fmt.Fprint(os.Stdout, art)
	}
	line := fmt.Sprintf("H Y D R A   -   v%s", version)
	fmt.Fprintf(os.Stdout, "%s\n\n", centered(line, w))
}

// PrintReady prints the "started" summary, centered to the same width as the
// logo so it reads as one coherent block. On first run it points at the UI
// rather than handing out a password: there is no password yet to hand out.
func PrintReady(host string, port int, firstRun bool) {
	_, w := bannerLayout()
	var b strings.Builder
	bar := "  " + strings.Repeat("-", w) + "\n"
	url := fmt.Sprintf("http://%s:%d", host, port)
	b.WriteString(bar)
	b.WriteString(centered(fmt.Sprintf("Engines started       API  %s", url), w) + "\n")
	b.WriteString(bar)
	b.WriteString("\n")
	if firstRun {
		lines := []string{
			"FIRST RUN - no admin account yet",
			"open " + url + " to create one",
		}
		bw := 0
		for _, l := range lines {
			if len(l) > bw {
				bw = len(l)
			}
		}
		box := []string{"+" + strings.Repeat("-", bw+2) + "+"}
		for _, l := range lines {
			box = append(box, fmt.Sprintf("| %-*s |", bw, l))
		}
		box = append(box, "+"+strings.Repeat("-", bw+2)+"+")
		for _, l := range box {
			b.WriteString(centered(l, w) + "\n")
		}
	} else {
		b.WriteString(centered("Login already set. Lost the password? run: hydra reset-password <new>", w) + "\n")
	}
	logsLine := "UI \"Logs\" tab"
	if dest := Default.MirrorName(); dest != "" {
		logsLine = "Detailed logs -> " + dest + "   |   " + logsLine
	}
	b.WriteString("\n" + centered(logsLine, w) + "\n\n")
	fmt.Fprint(os.Stdout, b.String())
}
