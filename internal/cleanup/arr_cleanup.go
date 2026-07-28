package cleanup

import (
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"math"
	"net/http"
	"regexp"
	"strings"
	"time"
	"unicode"

	"github.com/Kheopsian/hydra/internal/config"

	"golang.org/x/text/unicode/norm"
)

// ---------------------------------------------------------------------------
// Name normalisation
// ---------------------------------------------------------------------------

var (
	noiseRe = regexp.MustCompile(`(?i)\b(` +
		`2160p|1080p|1080i|720p|480p|4k|uhd|fhd|` +
		`hdr(?:10)?(?:\+|plus)?|dv|dolby\.?vision|` +
		`blu\.?ray|bdrip|bdremux|bd|` +
		`web(?:\.?(?:dl|rip))?|webrip|hdlight|hdrip|dvdrip|dvd|sdr|` +
		`x26[45]|h\.?26[45]|hevc|avc|av1|xvid|` +
		`aac|ac3|dts(?:\.?hd)?|truehd|atmos|flac|eac3|ma\.?5\.1|` +
		`10bits?|remux|hybrid|repack|proper|extended|theatrical|uncut|` +
		`multi|vff|vfi|vfq|vostfr|french|english|truefrench|` +
		`4klight|hdlight|hd|sd|` +
		`s\d{1,2}e\d{1,2}(?:[.\s-]e\d{1,2})*|s\d{1,2}` +
		`)\b`)
	separatorsRe = regexp.MustCompile(`[._\-]+`)
	groupsRe     = regexp.MustCompile(`\s*-\s*\w+\s*$`)
	bracketsRe   = regexp.MustCompile(`[\[\(][^\]\)]*[\]\)]`)
	yearRe       = regexp.MustCompile(`\b((?:19|20)\d{2})\b`)
	spacesRe     = regexp.MustCompile(`\s{2,}`)
	extRe        = regexp.MustCompile(`(?i)\.(mkv|mp4|avi|m4v|ts|nfo|txt)$`)
)

func normalize(name string) (string, int) {
	name = extRe.ReplaceAllString(name, "")
	yearMatch := yearRe.FindStringSubmatch(name)
	year := 0
	if len(yearMatch) > 1 {
		fmt.Sscanf(yearMatch[1], "%d", &year)
	}
	name = bracketsRe.ReplaceAllString(name, " ")
	name = groupsRe.ReplaceAllString(name, "")
	name = separatorsRe.ReplaceAllString(name, " ")
	name = yearRe.ReplaceAllString(name, " ")
	name = noiseRe.ReplaceAllString(name, " ")

	// NFKD decompose + strip combining marks
	result := norm.NFKD.String(name)
	var cleaned []rune
	for _, r := range result {
		if !unicode.Is(unicode.Mn, r) { // Mn = Mark, Nonspacing (combining)
			cleaned = append(cleaned, r)
		}
	}
	name = string(cleaned)

	name = spacesRe.ReplaceAllString(name, " ")
	name = strings.TrimSpace(strings.ToLower(name))
	return name, year
}

func score(torrentNorm, arrTitle string) float64 {
	tNorm, _ := normalize(arrTitle)
	if tNorm == "" || torrentNorm == "" {
		return 0
	}
	if torrentNorm == tNorm {
		return 1.0
	}
	if strings.HasPrefix(torrentNorm, tNorm) || strings.HasPrefix(tNorm, torrentNorm) {
		shorter := math.Min(float64(len(torrentNorm)), float64(len(tNorm)))
		longer := math.Max(float64(len(torrentNorm)), float64(len(tNorm)))
		return shorter / longer
	}

	tWords := toSet(strings.Fields(tNorm))
	sWords := toSet(strings.Fields(torrentNorm))
	if len(tWords) == 0 || len(sWords) == 0 {
		return 0
	}
	common := 0
	for w := range tWords {
		if sWords[w] {
			common++
		}
	}
	maxLen := len(tWords)
	if len(sWords) > maxLen {
		maxLen = len(sWords)
	}
	return float64(common) / float64(maxLen)
}

func toSet(s []string) map[string]bool {
	m := make(map[string]bool, len(s))
	for _, v := range s {
		m[v] = true
	}
	return m
}

// ---------------------------------------------------------------------------
// HoardEngineForCleanup is the minimal interface needed from the hoard engine.
// ---------------------------------------------------------------------------

// HoardEngineForCleanup provides torrent list and removal for cleanup.
type HoardEngineForCleanup interface {
	GetTorrentList() []map[string]interface{}
	RemoveTorrent(infoHash string, deleteFiles bool) error
}

// ---------------------------------------------------------------------------
// ArrCleanup
// ---------------------------------------------------------------------------

// ArrCleanup identifies torrents removed from the tracker that already have a
// replacement in Radarr/Sonarr (hasFile=true). Safe candidates can then be
// bulk-deleted.
type ArrCleanup struct {
	hoard  HoardEngineForCleanup
	cfg    config.ArrCleanupConfig
	client *http.Client

	radarrIndex []map[string]interface{}
	sonarrIndex []map[string]interface{}
}

// NewArrCleanup creates an ArrCleanup instance.
func NewArrCleanup(hoard HoardEngineForCleanup, cfg config.ArrCleanupConfig) *ArrCleanup {
	return &ArrCleanup{
		hoard:  hoard,
		cfg:    cfg,
		client: &http.Client{Timeout: 15 * time.Second},
	}
}

// ---------------------------------------------------------------------------
// Public API — implements ArrCleanupService interface
// ---------------------------------------------------------------------------

// Scan refreshes the Radarr/Sonarr index and returns cleanup candidates.
func (a *ArrCleanup) Scan() map[string]interface{} {
	// Refresh index
	a.refreshIndex()

	torrents := a.hoard.GetTorrentList()
	var candidates []map[string]interface{}

	for _, t := range torrents {
		trackerError, _ := t["tracker_error"].(bool)
		if !trackerError {
			continue
		}
		msg, _ := t["tracker_error_msg"].(string)
		msgLower := strings.ToLower(msg)
		if !strings.Contains(msgLower, "introuvable") &&
			!strings.Contains(msgLower, "supprime") &&
			!strings.Contains(msgLower, "supprimé") {
			continue
		}

		match := a.findMatch(t)
		entry := map[string]interface{}{
			"info_hash":         mapStr(t, "info_hash"),
			"name":              mapStr(t, "name"),
			"category":          mapStr(t, "category"),
			"tracker_error_msg": msg,
			"total_size":        t["total_size"],
			"safe_to_remove":    false,
		}
		if match != nil {
			entry["match"] = match
			if hasFile, ok := match["has_file"].(bool); ok && hasFile {
				entry["safe_to_remove"] = true
			}
		}
		candidates = append(candidates, entry)
	}

	return map[string]interface{}{
		"candidates": candidates,
		"count":      len(candidates),
	}
}

// Execute removes the specified torrents.
func (a *ArrCleanup) Execute(params map[string]interface{}) map[string]interface{} {
	infoHashesRaw, _ := params["info_hashes"].([]interface{})
	deleteFiles, _ := params["delete_files"].(bool)

	var removed []string
	var errors []map[string]interface{}

	for _, ih := range infoHashesRaw {
		hash, ok := ih.(string)
		if !ok {
			continue
		}
		if err := a.hoard.RemoveTorrent(hash, deleteFiles); err != nil {
			errors = append(errors, map[string]interface{}{
				"info_hash": hash, "error": err.Error(),
			})
		} else {
			removed = append(removed, hash)
		}
	}

	return map[string]interface{}{
		"removed": len(removed),
		"errors":  errors,
	}
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

func (a *ArrCleanup) refreshIndex() {
	a.radarrIndex = a.fetchArr(a.cfg.RadarrURL, a.cfg.RadarrAPIKey, "movie")
	a.sonarrIndex = a.fetchArr(a.cfg.SonarrURL, a.cfg.SonarrAPIKey, "series")
	slog.Info("arr_cleanup: index refreshed",
		"radarr", len(a.radarrIndex),
		"sonarr", len(a.sonarrIndex),
	)
}

func (a *ArrCleanup) fetchArr(baseURL, apiKey, endpoint string) []map[string]interface{} {
	if baseURL == "" || apiKey == "" {
		return nil
	}
	url := fmt.Sprintf("%s/api/v3/%s?apikey=%s", baseURL, endpoint, apiKey)
	resp, err := a.client.Get(url)
	if err != nil {
		slog.Warn("arr_cleanup: fetch failed", "url", url, "error", err)
		return nil
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	var data []map[string]interface{}
	json.Unmarshal(body, &data)
	return data
}

func (a *ArrCleanup) findMatch(torrent map[string]interface{}) map[string]interface{} {
	name, _ := torrent["name"].(string)
	normName, year := normalize(name)
	if normName == "" {
		return nil
	}

	category := strings.ToLower(mapStr(torrent, "category"))

	var match map[string]interface{}
	switch {
	case category == "movies" || category == "films" || category == "la cale":
		match = a.matchRadarr(normName, year)
		if match == nil {
			match = a.matchSonarr(normName, year)
		}
	case category == "series" || category == "animes" || category == "anime" || category == "remarkable":
		match = a.matchSonarr(normName, year)
		if match == nil {
			match = a.matchRadarr(normName, year)
		}
	default:
		r := a.matchRadarr(normName, year)
		s := a.matchSonarr(normName, year)
		if r != nil && s != nil {
			if r["score"].(float64) >= s["score"].(float64) {
				match = r
			} else {
				match = s
			}
		} else if r != nil {
			match = r
		} else {
			match = s
		}
	}

	return match
}

func (a *ArrCleanup) matchRadarr(normName string, year int) map[string]interface{} {
	var best map[string]interface{}
	var bestScore float64

	for _, m := range a.radarrIndex {
		titles := []string{mapStr(m, "title"), mapStr(m, "originalTitle")}
		if alts, ok := m["alternateTitles"].([]interface{}); ok {
			for _, alt := range alts {
				if am, ok := alt.(map[string]interface{}); ok {
					titles = append(titles, mapStr(am, "title"))
				}
			}
		}

		for _, title := range titles {
			if title == "" {
				continue
			}
			s := score(normName, title)
			if s < a.cfg.MinScore {
				continue
			}
			mYear := toIntVal(m["year"])
			if year > 0 && mYear > 0 && abs(mYear-year) > 1 {
				continue
			}
			if s > bestScore {
				bestScore = s
				hasFile, _ := m["hasFile"].(bool)
				best = map[string]interface{}{
					"source":   "radarr",
					"title":    mapStr(m, "title"),
					"year":     mYear,
					"has_file": hasFile,
					"arr_id":   toIntVal(m["id"]),
					"score":    s,
				}
			}
		}
	}
	return best
}

func (a *ArrCleanup) matchSonarr(normName string, year int) map[string]interface{} {
	var best map[string]interface{}
	var bestScore float64

	for _, s := range a.sonarrIndex {
		titles := []string{mapStr(s, "title")}
		if alts, ok := s["alternateTitles"].([]interface{}); ok {
			for _, alt := range alts {
				if am, ok := alt.(map[string]interface{}); ok {
					titles = append(titles, mapStr(am, "title"))
				}
			}
		}

		for _, title := range titles {
			if title == "" {
				continue
			}
			sc := score(normName, title)
			if sc < a.cfg.MinScore {
				continue
			}
			sYear := toIntVal(s["year"])
			if year > 0 && sYear > 0 && abs(sYear-year) > 1 {
				continue
			}
			if sc > bestScore {
				bestScore = sc
				epCount := 0
				if stats, ok := s["statistics"].(map[string]interface{}); ok {
					epCount = toIntVal(stats["episodeFileCount"])
				}
				best = map[string]interface{}{
					"source":   "sonarr",
					"title":    mapStr(s, "title"),
					"year":     sYear,
					"has_file": epCount > 0,
					"arr_id":   toIntVal(s["id"]),
					"score":    sc,
				}
			}
		}
	}
	return best
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

func mapStr(m map[string]interface{}, key string) string {
	if v, ok := m[key].(string); ok {
		return v
	}
	return ""
}

func toIntVal(v interface{}) int {
	switch n := v.(type) {
	case int:
		return n
	case int64:
		return int(n)
	case float64:
		return int(n)
	case float32:
		return int(n)
	default:
		return 0
	}
}

func abs(x int) int {
	if x < 0 {
		return -x
	}
	return x
}
