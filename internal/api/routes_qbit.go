package api

import (
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/store"
	"github.com/Kheopsian/hydra/internal/tagstore"
)

// ---------------------------------------------------------------------------
// Route registration — qBittorrent v2 API compatibility shim
// ---------------------------------------------------------------------------

func (s *Server) registerQbitRoutes() {
	v2 := s.router.Group("/api/v2")

	// Auth — always succeed (stateless, autobrr just needs the SID cookie)
	v2.POST("/auth/login", s.qbitAuthLogin)
	v2.POST("/auth/logout", s.qbitAuthLogout)

	// App info (Any = GET+POST, cross-seed uses POST for everything)
	v2.Any("/app/version", s.qbitAppVersion)
	v2.Any("/app/webapiVersion", s.qbitWebapiVersion)
	v2.Any("/app/buildInfo", s.qbitBuildInfo)
	v2.Any("/app/preferences", s.qbitPreferences)
	v2.POST("/app/setPreferences", s.qbitSetPreferences)

	// Transfer
	v2.Any("/transfer/info", s.qbitTransferInfo)

	// Torrents
	v2.Any("/torrents/info", s.qbitTorrentsInfo)
	v2.Any("/torrents/properties", s.qbitTorrentsProperties)
	v2.Any("/torrents/files", s.qbitTorrentsFiles)
	v2.Any("/torrents/trackers", s.qbitTorrentsTrackers)
	v2.POST("/torrents/add", s.qbitTorrentsAdd)
	v2.POST("/torrents/delete", s.qbitTorrentsDelete)
	v2.POST("/torrents/pause", s.qbitTorrentsPause)
	v2.POST("/torrents/resume", s.qbitTorrentsResume)
	// qBittorrent 5 renamed these verbs. Both spellings are live: clients
	// written against 5.x call stop/start, everything older calls pause/resume.
	v2.POST("/torrents/stop", s.qbitTorrentsPause)
	v2.POST("/torrents/start", s.qbitTorrentsResume)
	v2.POST("/torrents/recheck", s.qbitTorrentsRecheck)
	v2.POST("/torrents/setCategory", s.qbitTorrentsSetCategory)
	v2.POST("/torrents/addTrackers", s.qbitTorrentsAddTrackers)
	v2.POST("/torrents/removeTrackers", s.qbitTorrentsRemoveTrackers)
	v2.POST("/torrents/reannounce", s.qbitTorrentsReannounce)

	// Categories
	v2.GET("/torrents/categories", s.qbitCategoriesGet)
	v2.POST("/torrents/createCategory", s.qbitCategoryCreate)
	v2.POST("/torrents/editCategory", s.qbitCategoryEdit)
	v2.POST("/torrents/removeCategories", s.qbitCategoriesRemove)
	v2.GET("/torrents/tags", s.qbitTorrentsTags)
	v2.POST("/torrents/createTags", s.qbitCreateTags)
	v2.POST("/torrents/deleteTags", s.qbitDeleteTags)
	v2.POST("/torrents/addTags", s.qbitAddTags)
	v2.POST("/torrents/removeTags", s.qbitRemoveTags)
}

// ===========================================================================
// Auth
// ===========================================================================

func (s *Server) qbitAuthLogin(c *gin.Context) {
	// Always succeed — set SID cookie so autobrr is happy
	c.SetCookie("SID", "hydra-session-token", 3600*24, "/", "", false, true)
	c.String(http.StatusOK, "Ok.")
}

func (s *Server) qbitAuthLogout(c *gin.Context) {
	c.SetCookie("SID", "", -1, "/", "", false, true)
	c.String(http.StatusOK, "")
}

// ===========================================================================
// App Info
// ===========================================================================

func (s *Server) qbitAppVersion(c *gin.Context) {
	c.String(http.StatusOK, "v4.6.0")
}

func (s *Server) qbitWebapiVersion(c *gin.Context) {
	c.String(http.StatusOK, "2.9.3")
}

func (s *Server) qbitBuildInfo(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{
		"qt":         "6.5.3",
		"libtorrent": "2.0.9.0",
		"boost":      "1.83.0",
		"openssl":    "3.1.4",
		"bitness":    64,
	})
}

func (s *Server) qbitPreferences(c *gin.Context) {
	// The live port, not the boot config: a port-forward script reads this back
	// to confirm what it just set.
	listenPort := s.config.Race.ListenPort
	if s.raceEngine != nil {
		if p := s.raceEngine.ListenPort(); p > 0 {
			listenPort = p
		}
	}
	c.JSON(http.StatusOK, gin.H{
		"save_path":                 "/downloads",
		"temp_path_enabled":         false,
		"listen_port":               listenPort,
		"max_connec":                s.config.Race.MaxConnections,
		"max_uploads_per_torrent":   s.config.Race.MaxUploadsPerTorrent,
		"dht":                       false,
		"pex":                       false,
		"lsd":                       false,
		"encryption":                1,
		"queueing_enabled":          false,
		"max_active_downloads":      s.config.Race.ActiveDownloads,
		"max_active_uploads":        s.config.Race.ActiveSeeds,
		"max_active_torrents":       s.config.Race.ActiveLimit,
		"web_ui_port":               s.config.Daemon.APIPort,
		"locale":                    "en",
		"create_subfolder_enabled":  s.config.Daemon.CreateTorrentFolder,
		"add_trackers_enabled":      false,
		"alternative_webui_enabled": false,
	})
}

// qbitSetPreferences accepts the qBittorrent settings write. It exists for the
// one preference Hydra can genuinely apply at runtime — listen_port — which is
// how every VPN port-forward script in the wild pushes a rotated port. Other
// keys are accepted and ignored, as qBittorrent itself does with preferences a
// build does not support: a client sending a full settings blob must not get an
// error over a field we do not model.
//
// Body shape follows qBittorrent: a form field `json` holding the changed
// preferences. A raw JSON body is accepted too, since some clients send that.
func (s *Server) qbitSetPreferences(c *gin.Context) {
	var prefs map[string]interface{}

	if raw := c.PostForm("json"); raw != "" {
		if err := json.Unmarshal([]byte(raw), &prefs); err != nil {
			c.String(http.StatusBadRequest, "invalid json field: %v", err)
			return
		}
	} else if err := c.ShouldBindJSON(&prefs); err != nil {
		c.String(http.StatusBadRequest, "expected a `json` form field or a JSON body: %v", err)
		return
	}

	raw, ok := prefs["listen_port"]
	if !ok {
		c.Status(http.StatusOK)
		return
	}
	// JSON numbers decode to float64; a script sending "51413" as a string is
	// common enough to be worth accepting.
	var port int
	switch v := raw.(type) {
	case float64:
		port = int(v)
	case string:
		n, err := strconv.Atoi(v)
		if err != nil {
			c.String(http.StatusBadRequest, "listen_port %q is not a number", v)
			return
		}
		port = n
	default:
		c.String(http.StatusBadRequest, "listen_port must be a number")
		return
	}
	if port <= 0 || port > 65535 {
		c.String(http.StatusBadRequest, "listen_port out of range (1-65535)")
		return
	}
	if s.raceEngine == nil {
		c.String(http.StatusServiceUnavailable, "engine unavailable")
		return
	}
	// Reported rather than swallowed: a port-forward script that believes a
	// failed rebind leaves the node unreachable with nothing to correct it.
	if err := s.raceEngine.SetListenPort(port); err != nil {
		c.String(http.StatusInternalServerError, "listen_port rebind failed: %v", err)
		return
	}
	if err := s.persistListenPort("race", port); err != nil {
		slog.Warn("qbit setPreferences: port rebound but not persisted", "port", port, "err", err)
	}
	c.Status(http.StatusOK)
}

// ===========================================================================
// Transfer
// ===========================================================================

func (s *Server) qbitTransferInfo(c *gin.Context) {
	var dlSpeed, ulSpeed int64

	if s.raceEngine != nil {
		for _, t := range s.raceEngine.GetAllStatus() {
			if v, ok := t["download_rate"].(int64); ok {
				dlSpeed += v
			}
			if v, ok := t["upload_rate"].(int64); ok {
				ulSpeed += v
			}
		}
	}

	if s.hoardEngine != nil {
		for _, t := range s.hoardEngine.GetTorrentList() {
			if v, ok := t["download_rate"].(int64); ok {
				dlSpeed += v
			}
			if v, ok := t["upload_rate"].(int64); ok {
				ulSpeed += v
			}
		}
	}

	sessionUL, sessionDL := getSessionDelta()

	c.JSON(http.StatusOK, gin.H{
		"dl_info_speed":     dlSpeed,
		"dl_info_data":      sessionDL,
		"up_info_speed":     ulSpeed,
		"up_info_data":      sessionUL,
		"dl_rate_limit":     0,
		"up_rate_limit":     0,
		"dht_nodes":         0,
		"connection_status": "connected",
	})
}

// ===========================================================================
// Torrents
// ===========================================================================

func (s *Server) qbitTorrentsInfo(c *gin.Context) {
	filter := c.DefaultPostForm("filter", c.DefaultQuery("filter", "all"))
	categoryFilter := c.DefaultPostForm("category", c.Query("category"))
	tagFilter := c.DefaultPostForm("tag", c.Query("tag"))
	hashesFilter := c.DefaultPostForm("hashes", c.Query("hashes"))
	sortField := c.DefaultPostForm("sort", c.DefaultQuery("sort", "added_on"))
	reverse := c.DefaultPostForm("reverse", c.DefaultQuery("reverse", "false")) == "true"
	limitStr := c.DefaultPostForm("limit", c.DefaultQuery("limit", "0"))
	offsetStr := c.DefaultPostForm("offset", c.DefaultQuery("offset", "0"))

	limit, _ := strconv.Atoi(limitStr)
	offset, _ := strconv.Atoi(offsetStr)

	// Build hash set for filtering if hashes parameter is provided.
	var hashSet map[string]bool
	if hashesFilter != "" {
		hashSet = make(map[string]bool)
		for _, h := range strings.Split(hashesFilter, "|") {
			h = strings.TrimSpace(strings.ToLower(h))
			if h != "" {
				hashSet[h] = true
			}
		}
	}

	// Non-nil: an empty listing must marshal as [], never null. qBittorrent
	// always answers with an array and clients dereference it directly --
	// cross-seed calls torrents.find(...) straight on the parsed body, so a
	// null throws there instead of reading as "no torrents". The window is
	// real: at boot, before the engines have restored their state, the
	// default filter=all skips every filter block and the slice stays empty.
	// Built once and shared for a couple of seconds when qbit_snapshot is on:
	// the *arr stack, cross-seed and autobrr all poll this endpoint at a rate
	// we do not control, and each poll was rebuilding a map per torrent.
	// qbitSnapshot hands back a copy of the slice (the sort below is in place),
	// sharing the maps, which are read-only once built.
	// A category poll builds only that category. The hoard side filters on the
	// struct before building any row, which is where the cost is; race and the
	// pending magnets are small enough to build and filter. Same rows as the
	// full path would have yielded after its own category filter, so the block
	// below is skipped rather than run twice.
	scope := ""
	if categoryFilter != "" {
		scope = "cat:" + categoryFilter
	}
	allTorrents := qbitSnapshotFor(scope, func() []map[string]interface{} {
		// Non-nil: an empty listing must marshal as [], never null.
		out := make([]map[string]interface{}, 0)
		keep := func(row map[string]interface{}) {
			if categoryFilter != "" {
				if cat, _ := row["category"].(string); cat != categoryFilter {
					return
				}
			}
			out = append(out, row)
		}
		// Magnets whose metadata has not landed yet: qBit calls this metaDL,
		// and clients poll for it rather than treating the grab as lost.
		for _, p := range PendingMagnets() {
			keep(p.QbitRow())
		}
		if s.raceEngine != nil {
			for _, t := range s.raceEngine.GetAllStatus() {
				keep(hydraToQbitTorrent(t, "race"))
			}
		}
		if s.hoardEngine != nil {
			if categoryFilter != "" {
				for _, t := range s.hoardEngine.GetTorrentListInCategory(categoryFilter) {
					out = append(out, hydraToQbitTorrent(t, "hoard"))
				}
			} else {
				for _, t := range s.hoardEngine.GetTorrentList() {
					out = append(out, hydraToQbitTorrent(t, "hoard"))
				}
			}
		}
		return out
	})

	// Filter by hashes
	if hashSet != nil {
		filtered := make([]map[string]interface{}, 0)
		for _, t := range allTorrents {
			if h, _ := t["hash"].(string); hashSet[h] {
				filtered = append(filtered, t)
			}
		}
		allTorrents = filtered
	}

	// Filter by state
	if filter != "all" {
		filtered := make([]map[string]interface{}, 0)
		for _, t := range allTorrents {
			state, _ := t["state"].(string)
			if filterStateMatch(state, filter) {
				filtered = append(filtered, t)
			}
		}
		allTorrents = filtered
	}

	// Category filtering already happened inside the scoped build above.

	// Filter by tag (qBittorrent: torrents having the given tag)
	if tagFilter != "" {
		filtered := make([]map[string]interface{}, 0)
		for _, t := range allTorrents {
			if qbitTagsContains(t, tagFilter) {
				filtered = append(filtered, t)
			}
		}
		allTorrents = filtered
	}

	// Sort
	sort.Slice(allTorrents, func(i, j int) bool {
		vi := allTorrents[i][sortField]
		vj := allTorrents[j][sortField]
		less := compareValues(vi, vj)
		if reverse {
			return !less
		}
		return less
	})

	// Pagination
	total := len(allTorrents)
	if offset > 0 && offset < total {
		allTorrents = allTorrents[offset:]
	}
	if limit > 0 && limit < len(allTorrents) {
		allTorrents = allTorrents[:limit]
	}

	c.JSON(http.StatusOK, allTorrents)
}

func (s *Server) qbitTorrentsProperties(c *gin.Context) {
	hash := strings.ToLower(c.DefaultPostForm("hash", c.Query("hash")))
	if hash == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "hash required"})
		return
	}

	var detail map[string]interface{}
	if s.raceEngine != nil && s.raceEngine.HasTorrent(hash) {
		detail = s.raceEngine.GetTorrentDetail(hash)
	} else if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
		detail = s.hoardEngine.GetTorrentDetail(hash)
	}

	if detail == nil {
		c.JSON(http.StatusNotFound, gin.H{})
		return
	}

	// properties.save_path is the directory the content root sits in —
	// i.e. the engine save_path, same as torrents/info reports. See the
	// path-semantics note in hydraToQbitTorrent.
	savePath := getStr(detail, "engine_save_path")
	if savePath == "" {
		savePath = getStr(detail, "save_path")
	}

	// Map to qBit properties format
	props := gin.H{
		"save_path":                savePath,
		"creation_date":            getInt64(detail, "added_time"),
		"piece_size":               getInt64(detail, "piece_length"),
		"comment":                  "",
		"total_wasted":             0,
		"total_uploaded":           getInt64(detail, "total_uploaded"),
		"total_uploaded_session":   getInt64(detail, "total_uploaded"),
		"total_downloaded":         getInt64(detail, "total_downloaded"),
		"total_downloaded_session": getInt64(detail, "total_downloaded"),
		"up_limit":                 -1,
		"dl_limit":                 -1,
		"time_elapsed":             time.Now().Unix() - getInt64(detail, "added_time"),
		"seeding_time":             getInt64(detail, "seeding_time"),
		"nb_connections":           getInt64(detail, "num_peers"),
		"share_ratio":              getRatio(detail),
		"addition_date":            getInt64(detail, "added_time"),
		"completion_date":          getInt64(detail, "completed_time"),
		"created_by":               "",
		"dl_speed_avg":             0,
		"dl_speed":                 getInt64(detail, "download_rate"),
		"eta":                      getETA(detail),
		"last_seen":                time.Now().Unix(),
		"peers":                    getInt64(detail, "num_peers"),
		"peers_total":              getInt64(detail, "num_peers"),
		"seeds":                    getInt64(detail, "num_seeds"),
		"seeds_total":              getInt64(detail, "num_seeds"),
		"total_size":               getInt64(detail, "total_size"),
		"up_speed_avg":             0,
		"up_speed":                 getInt64(detail, "upload_rate"),
	}

	c.JSON(http.StatusOK, props)
}

func (s *Server) qbitTorrentsFiles(c *gin.Context) {
	hash := strings.ToLower(c.DefaultPostForm("hash", c.Query("hash")))
	if hash == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "hash required"})
		return
	}

	var detail map[string]interface{}
	var engineFiles []map[string]interface{}
	if s.raceEngine != nil && s.raceEngine.HasTorrent(hash) {
		detail = s.raceEngine.GetTorrentDetail(hash)
		engineFiles = s.raceEngine.GetTorrentFileList(hash)
	} else if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
		detail = s.hoardEngine.GetTorrentDetail(hash)
		engineFiles = s.hoardEngine.GetTorrentFileList(hash)
	}

	if detail == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}

	// Engine file paths are BEP-3 relative paths: for a multi-file torrent
	// they are relative to the info.name directory, which Typhon creates
	// under the engine save_path; for a single-file torrent the one path
	// IS info.name, sitting directly in the engine save_path. torrents/info
	// reports save_path = engine save_path, so the names here must carry
	// the info.name directory for multi-file torrents and nothing extra for
	// single-file ones — anything else makes cross-seed's
	// `save_path + files[i].name` miss the file on disk.
	name := getStr(detail, "name")
	prefix := ""
	if b, _ := detail["multi_file"].(bool); b {
		prefix = name + "/"
	}

	// Typhon reports the real file list; fall back to the single-file
	// shape only when it is unavailable (engine down, torrent metadata
	// not loaded yet).
	if len(engineFiles) == 0 {
		engineFiles = []map[string]interface{}{{
			"path": name,
			"size": getInt64(detail, "total_size"),
		}}
		prefix = ""
	}

	progress := getFloat(detail, "progress")
	qbitFiles := make([]gin.H, 0, len(engineFiles))
	for i, f := range engineFiles {
		qbitFiles = append(qbitFiles, gin.H{
			"index":        i,
			"name":         prefix + getStr(f, "path"),
			"size":         getInt64(f, "size"),
			"progress":     progress,
			"priority":     1,
			"is_seed":      false,
			"piece_range":  []int{0, 0},
			"availability": 1.0,
		})
	}
	c.JSON(http.StatusOK, qbitFiles)
}

func (s *Server) qbitTorrentsTrackers(c *gin.Context) {
	hash := strings.ToLower(c.DefaultPostForm("hash", c.Query("hash")))
	if hash == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "hash required"})
		return
	}

	var detail map[string]interface{}
	if s.raceEngine != nil && s.raceEngine.HasTorrent(hash) {
		detail = s.raceEngine.GetTorrentDetail(hash)
	} else if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
		detail = s.hoardEngine.GetTorrentDetail(hash)
	}

	if detail == nil {
		c.JSON(http.StatusNotFound, gin.H{})
		return
	}

	result := []gin.H{
		{
			"url":            "** [DHT] **",
			"status":         2,
			"tier":           "",
			"num_peers":      0,
			"num_seeds":      0,
			"num_leeches":    0,
			"num_downloaded": 0,
			"msg":            "",
		},
		{
			"url":            "** [PeX] **",
			"status":         2,
			"tier":           "",
			"num_peers":      0,
			"num_seeds":      0,
			"num_leeches":    0,
			"num_downloaded": 0,
			"msg":            "",
		},
		{
			"url":            "** [LSD] **",
			"status":         2,
			"tier":           "",
			"num_peers":      0,
			"num_seeds":      0,
			"num_leeches":    0,
			"num_downloaded": 0,
			"msg":            "",
		},
	}

	if trackers, ok := detail["trackers"].([]interface{}); ok {
		for _, raw := range trackers {
			tr, ok := raw.(map[string]interface{})
			if !ok {
				continue
			}
			status := 2 // working
			msg := getStr(tr, "message")
			if msg != "" {
				status = 4 // not working
			}
			result = append(result, gin.H{
				"url":            getStr(tr, "url"),
				"status":         status,
				"tier":           getInt64(tr, "tier"),
				"num_peers":      0,
				"num_seeds":      getInt64(tr, "scrape_complete"),
				"num_leeches":    getInt64(tr, "scrape_incomplete"),
				"num_downloaded": 0,
				"msg":            msg,
			})
		}
	}

	c.JSON(http.StatusOK, result)
}

func (s *Server) qbitTorrentsAdd(c *gin.Context) {
	// Parse multipart form
	if err := c.Request.ParseMultipartForm(32 << 20); err != nil {
		c.String(http.StatusBadRequest, "Bad request")
		return
	}

	savePath := c.PostForm("savepath")
	category := c.PostForm("category")
	tags := c.PostForm("tags")
	// qBit's `skip_checking=true` means "trust the on-disk payload, do
	// not hash-verify". Cross-seed sets this on every inject because it
	// has just hardlinked the staged payload into linkDir and the new
	// torrent's pieces should map to the existing bytes 1:1. Without it,
	// Hydra/Typhon would fall back to a fresh download instead of
	// recognising the pre-staged content and going straight to seed.
	skipChecking := c.PostForm("skip_checking") == "true"

	// Determine mode from category
	mode := "race"
	cats := loadCategories(s.config.Daemon.DataDir)
	for _, cat := range cats {
		if strings.EqualFold(cat.Name, category) {
			mode = cat.Mode
			if savePath == "" {
				savePath = cat.SavePath
			}
			break
		}
	}

	if mode == "race" {
		s.raceDrainOnAddIfFull()
	}

	// Check for magnet URLs first
	urls := c.PostForm("urls")
	if urls != "" {
		for _, magnetURI := range strings.Split(urls, "\n") {
			magnetURI = strings.TrimSpace(magnetURI)
			if magnetURI == "" {
				continue
			}

			// Resolve through the shared magnet path rather than poking an
			// engine directly: it returns as soon as the job starts (an *arr
			// grab must not block on a swarm), and hoard gets magnets too --
			// the old hoard branch dropped the URI on the floor and added
			// nothing.
			if _, err := s.startMagnetResolve("local", mode, magnetURI, savePath, category, nil, nil); err != nil {
				c.String(http.StatusInternalServerError, "Fails.")
				return
			}
		}
	}

	// Check for uploaded .torrent files
	form := c.Request.MultipartForm
	if form != nil && form.File != nil {
		for _, fileHeaders := range form.File {
			for _, fh := range fileHeaders {
				f, err := fh.Open()
				if err != nil {
					continue
				}

				tmpDir := filepath.Join(s.config.Daemon.DataDir, "uploads")
				os.MkdirAll(tmpDir, 0755)
				tmpPath := filepath.Join(tmpDir, fh.Filename)

				out, err := os.Create(tmpPath)
				if err != nil {
					f.Close()
					continue
				}
				io.Copy(out, f)
				out.Close()
				f.Close()

				switch mode {
				case "race":
					if s.raceEngine != nil {
						if skipChecking {
							s.raceEngine.AddTorrentSeedMode(tmpPath, savePath, category)
						} else {
							s.raceEngine.AddTorrent(tmpPath, "", savePath, nil, category)
						}
					}
				case "hoard":
					if s.hoardEngine != nil {
						var ih string
						if skipChecking {
							ih, _ = s.hoardEngine.AddTorrentSeedMode(tmpPath, savePath, category)
						} else {
							ih, _ = s.hoardEngine.AddTorrent(tmpPath, savePath, category)
						}
						if ih != "" && tags != "" {
							s.hoardEngine.AddTags(ih, splitTagsCSV(tags))
						}
					}
				}
			}
		}
	}

	if tags != "" {
		s.registerTags(splitTagsCSV(tags))
		s.persistTagsAll()
	}
	c.String(http.StatusOK, "Ok.")
}

func (s *Server) qbitTorrentsDelete(c *gin.Context) {
	hashes := c.PostForm("hashes")
	deleteFilesStr := c.PostForm("deleteFiles")
	deleteFiles := deleteFilesStr == "true" || deleteFilesStr == "1"

	for _, hash := range parseHashes(hashes) {
		// Stats absorption gérée par le callback OnBeforeRemove de l'engine.
		if s.raceEngine != nil && s.raceEngine.HasTorrent(hash) {
			if err := s.raceEngine.RemoveTorrent(hash, deleteFiles); err != nil {
				slog.Warn("qbit-shim: race remove returned error", "info_hash", hash, "delete_files", deleteFiles, "err", err)
			}
		}
		if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
			s.hoardEngine.RemoveTorrent(hash, deleteFiles)
		}
		if s.stateManager != nil {
			s.stateManager.RemoveTorrent(hash)
		}
	}

	c.String(http.StatusOK, "")
}

func (s *Server) qbitTorrentsPause(c *gin.Context)  { s.qbitSetPaused(c, true) }
func (s *Server) qbitTorrentsResume(c *gin.Context) { s.qbitSetPaused(c, false) }

// qbitSetPaused implements the qBittorrent pause/resume verbs. A pause arriving
// here counts as the user's own intent — autobrr, Sonarr and friends act on
// their behalf, and qBittorrent's semantics are that a paused torrent stays
// paused. So it is sticky and survives a restart, exactly like a click in the
// UI, and no scheduler will lift it.
//
// "hashes=all" is qBittorrent's wildcard for the whole library.
func (s *Server) qbitSetPaused(c *gin.Context, paused bool) {
	raw := c.PostForm("hashes")
	if raw == "all" {
		if s.hoardEngine != nil {
			if paused {
				s.hoardEngine.PauseAll()
			} else {
				s.hoardEngine.ResumeAll()
			}
			s.hoardEngine.MarkAllUserPaused(paused)
			persistPausedSession(store.Hoard, paused)
		}
		c.String(http.StatusOK, "")
		return
	}

	var hoardHit, raceHit []string
	for _, h := range parseHashes(raw) {
		switch {
		case s.hoardEngine != nil && s.hoardEngine.HasTorrent(h):
			if s.hoardEngine.SetUserPaused(h, paused) == nil {
				hoardHit = append(hoardHit, h)
			}
		case s.raceEngine != nil:
			if s.raceEngine.SetUserPaused(h, paused) == nil {
				raceHit = append(raceHit, h)
			}
		}
	}
	persistPaused(append(hoardHit, raceHit...), paused)
	c.String(http.StatusOK, "")
}

func (s *Server) qbitTorrentsSetCategory(c *gin.Context) {
	// Acknowledge but no-op for now — category is set at add time
	c.String(http.StatusOK, "")
}

// ===========================================================================
// Categories (qBit format)
// ===========================================================================

func (s *Server) qbitCategoriesGet(c *gin.Context) {
	cats := loadCategories(s.config.Daemon.DataDir)
	result := make(map[string]gin.H, len(cats))
	for _, cat := range cats {
		result[cat.Name] = gin.H{
			"name":     cat.Name,
			"savePath": cat.SavePath,
		}
	}
	c.JSON(http.StatusOK, result)
}

func (s *Server) qbitCategoryCreate(c *gin.Context) {
	name := c.PostForm("category")
	savePath := c.PostForm("savePath")

	if name == "" {
		c.String(http.StatusBadRequest, "category name required")
		return
	}

	cats := loadCategories(s.config.Daemon.DataDir)
	for _, existing := range cats {
		if existing.Name == name {
			c.String(http.StatusConflict, "category exists")
			return
		}
	}

	cats = append(cats, category{Name: name, SavePath: savePath, Mode: "hoard"})
	saveCategories(s.config.Daemon.DataDir, cats)

	c.String(http.StatusOK, "")
}

func (s *Server) qbitCategoryEdit(c *gin.Context) {
	name := c.PostForm("category")
	savePath := c.PostForm("savePath")

	cats := loadCategories(s.config.Daemon.DataDir)
	for i, cat := range cats {
		if cat.Name == name {
			cats[i].SavePath = savePath
			saveCategories(s.config.Daemon.DataDir, cats)
			c.String(http.StatusOK, "")
			return
		}
	}
	c.String(http.StatusNotFound, "category not found")
}

func (s *Server) qbitCategoriesRemove(c *gin.Context) {
	names := c.PostForm("categories")
	toRemove := make(map[string]bool)
	for _, name := range strings.Split(names, "\n") {
		name = strings.TrimSpace(name)
		if name != "" {
			toRemove[name] = true
		}
	}

	cats := loadCategories(s.config.Daemon.DataDir)
	newCats := make([]category, 0, len(cats))
	for _, cat := range cats {
		if !toRemove[cat.Name] {
			newCats = append(newCats, cat)
		}
	}
	saveCategories(s.config.Daemon.DataDir, newCats)

	c.String(http.StatusOK, "")
}

// ===========================================================================
// Helper functions
// ===========================================================================

// hydraStateToQbit maps Hydra's internal state string to qBittorrent state.
//
// userPaused separates the two ways a torrent can be sitting still, and the
// distinction is not cosmetic: to Sonarr, "stopped" means a human halted this
// deliberately, "queued" means it is waiting its turn and all is well, and
// "stalled" means the download is broken — which triggers its dead-download
// handling. A torrent held back by one of our schedulers is queued, never
// stalled.
func hydraStateToQbit(state string, progress float64, uploadRate, downloadRate int64, userPaused bool) string {
	if userPaused {
		if progress >= 1.0 {
			return "stoppedUP"
		}
		return "stoppedDL"
	}
	switch state {
	case "downloading":
		if downloadRate > 0 {
			return "downloading"
		}
		return "stalledDL"
	case "seeding":
		if uploadRate > 0 {
			return "uploading"
		}
		return "stalledUP"
	case "checking", "checking_files":
		return "checkingDL"
	case "stopped":
		// A human stopped this one. qBittorrent 5 calls that stopped; older
		// clients know it as paused, and both read it as "not my problem".
		if progress >= 1.0 {
			return "stoppedUP"
		}
		return "stoppedDL"
	case "queued", "paused":
		// A scheduler is holding this one back. It is not broken and nobody
		// asked for it to stop, so it is queued. ("paused" only turns up from
		// an engine snapshot that predates the intent flag being applied.)
		if progress >= 1.0 {
			return "queuedUP"
		}
		return "queuedDL"
	case "error":
		return "error"
	case "moving":
		return "moving"
	case "allocating":
		return "allocating"
	default:
		if progress >= 1.0 {
			return "stalledUP"
		}
		return "stalledDL"
	}
}

// hydraToQbitTorrent converts a Hydra torrent status map to qBittorrent format.
func hydraToQbitTorrent(t map[string]interface{}, engineName string) map[string]interface{} {
	progress := getFloat(t, "progress")
	dlRate := getInt64(t, "download_rate")
	ulRate := getInt64(t, "upload_rate")
	stateStr := getStr(t, "state")

	qbitState := hydraStateToQbit(stateStr, progress, ulRate, dlRate, getBool(t, "user_paused"))
	totalSize := getInt64(t, "total_size")
	downloaded := getInt64(t, "total_downloaded")
	uploaded := getInt64(t, "total_uploaded")

	addedOn := getInt64(t, "added_time")
	if addedOn == 0 {
		addedOn = time.Now().Unix()
	}

	cat := getStr(t, "category")
	if cat == "" {
		cat = engineName
	}

	// For completed torrents, Rain reports Bytes.Downloaded = 0 (it only
	// counts bytes downloaded during this session). Fix up the values so
	// *arr apps see consistent completion data.
	completed := downloaded
	amountLeft := totalSize - downloaded
	if progress >= 1.0 {
		completed = totalSize
		amountLeft = 0
		if downloaded == 0 {
			downloaded = totalSize
		}
	}

	ratio := float64(0)
	if downloaded > 0 {
		ratio = float64(uploaded) / float64(downloaded)
	}

	eta := int64(8640000) // default: infinity
	if dlRate > 0 && totalSize > 0 {
		if amountLeft > 0 {
			eta = amountLeft / dlRate
		} else {
			eta = 0
		}
	}

	// qBit clients (cross-seed in particular) reconstruct the on-disk
	// content path as save_path + name, and the per-file path as
	// save_path + files[i].name. Both must agree with what Typhon
	// actually writes, which is always
	//     <engine save_path>/<info.name if multi-file>/<file path>
	// for BOTH engines (see typhon-engine/src/disk/mod.rs). So the qBit
	// view is the plain BEP-3 one: save_path = the engine save_path,
	// name = info.name, content_path = the two joined. That holds for a
	// hoard wrapper folder (create_torrent_folder on -> the wrapper IS
	// the engine save_path) as much as for race's shared save_path.
	//
	// Do NOT derive name from basename(save_path): for a single-file
	// torrent the engine save_path is the containing directory, so that
	// yields the parent folder's name instead of the release, and the
	// matching prefix in qbitTorrentsFiles then doubles the directory
	// segment (`.../Torr9/Torr9/file.mkv`), which breaks every
	// cross-seed link built from the client torrent list.
	qbitName := getStr(t, "name")
	spDir := getStr(t, "engine_save_path")
	if spDir == "" {
		spDir = getStr(t, "save_path")
	}
	contentPath := filepath.Join(spDir, qbitName)
	return map[string]interface{}{
		"hash":           getStr(t, "info_hash"),
		"name":           qbitName,
		"state":          qbitState,
		"progress":       progress,
		"size":           totalSize,
		"total_size":     totalSize,
		"dlspeed":        dlRate,
		"upspeed":        ulRate,
		"num_seeds":      getInt64(t, "num_seeds"),
		"num_leechs":     getInt64(t, "num_peers"),
		"num_complete":   getInt64(t, "num_seeds"),
		"num_incomplete": getInt64(t, "num_peers"),
		"ratio":          math.Round(ratio*100) / 100,
		"eta":            eta,
		"added_on":       addedOn,
		"completion_on":  getInt64(t, "completed_time"),
		"category":       cat,
		"tags":           qbitTagsCSV(t),
		"save_path":      spDir,
		"content_path":   contentPath,
		"downloaded":     downloaded,
		"uploaded":       uploaded,
		"amount_left":    amountLeft,
		"completed":      completed,
		"seen_complete":  0,
		"priority":       0,
		"seq_dl":         false,
		"f_l_piece_prio": false,
		"auto_tmm":       false,
		"super_seeding":  false,
		"force_start":    false,
		"magnet_uri":     "",
		"time_active":    time.Now().Unix() - addedOn,
		"tracker":        getStr(t, "tracker"),
		"availability":   getFloat(t, "availability"),
	}
}

// filterStateMatch returns true if a qBit state name matches the given filter name.
func filterStateMatch(qbitState, filterName string) bool {
	switch filterName {
	case "all":
		return true
	case "downloading":
		return qbitState == "downloading" || qbitState == "stalledDL" ||
			qbitState == "checkingDL" || qbitState == "queuedDL" ||
			qbitState == "allocating"
	case "seeding":
		return qbitState == "uploading" || qbitState == "stalledUP" ||
			qbitState == "queuedUP" || qbitState == "checkingUP"
	case "completed":
		return qbitState == "uploading" || qbitState == "stalledUP" ||
			qbitState == "pausedUP" || qbitState == "stoppedUP" || qbitState == "queuedUP" ||
			qbitState == "checkingUP"
	case "paused", "stopped":
		// Same set under both spellings: qBittorrent 5 renamed the filter, and
		// we still answer the old name for everything written before it.
		return qbitState == "pausedDL" || qbitState == "pausedUP" ||
			qbitState == "stoppedDL" || qbitState == "stoppedUP"
	case "active":
		return qbitState == "downloading" || qbitState == "uploading"
	case "inactive":
		return qbitState == "stalledDL" || qbitState == "stalledUP" ||
			qbitState == "pausedDL" || qbitState == "pausedUP" ||
			qbitState == "stoppedDL" || qbitState == "stoppedUP"
	case "stalled":
		return qbitState == "stalledDL" || qbitState == "stalledUP"
	case "stalled_uploading":
		return qbitState == "stalledUP"
	case "stalled_downloading":
		return qbitState == "stalledDL"
	case "errored":
		return qbitState == "error"
	case "resumed", "running":
		return qbitState != "pausedDL" && qbitState != "pausedUP" &&
			qbitState != "stoppedDL" && qbitState != "stoppedUP"
	default:
		return true
	}
}

// parseHashes splits a pipe-separated or comma-separated hash string.
func parseHashes(hashesStr string) []string {
	hashesStr = strings.TrimSpace(hashesStr)
	if hashesStr == "" || hashesStr == "all" {
		return nil
	}

	var hashes []string
	// qBit uses pipe separator
	if strings.Contains(hashesStr, "|") {
		hashes = strings.Split(hashesStr, "|")
	} else {
		hashes = strings.Split(hashesStr, ",")
	}

	result := make([]string, 0, len(hashes))
	for _, h := range hashes {
		h = strings.TrimSpace(strings.ToLower(h))
		if h != "" {
			result = append(result, h)
		}
	}
	return result
}

// ---------------------------------------------------------------------------
// Value extraction helpers (safe access to map[string]interface{})
// ---------------------------------------------------------------------------

// ---- qBittorrent tags parity ----

// qbitTagsCSV renders a torrent map's tags ([]string) as a qBit CSV string.
func qbitTagsCSV(t map[string]interface{}) string {
	switch v := t["tags"].(type) {
	case []string:
		return strings.Join(v, ", ")
	case []interface{}:
		parts := make([]string, 0, len(v))
		for _, x := range v {
			if s, ok := x.(string); ok {
				parts = append(parts, s)
			}
		}
		return strings.Join(parts, ", ")
	}
	return ""
}

// qbitTagsContains reports whether a torrent map carries the given tag.
func qbitTagsContains(t map[string]interface{}, tag string) bool {
	switch v := t["tags"].(type) {
	case string:
		for _, s := range splitTagsCSV(v) {
			if s == tag {
				return true
			}
		}
	case []string:
		for _, s := range v {
			if s == tag {
				return true
			}
		}
	case []interface{}:
		for _, x := range v {
			if s, ok := x.(string); ok && s == tag {
				return true
			}
		}
	}
	return false
}

// splitTagsCSV splits a qBit CSV tag string into trimmed, non-empty names.
func splitTagsCSV(csv string) []string {
	var out []string
	for _, t := range strings.Split(csv, ",") {
		if t = strings.TrimSpace(t); t != "" {
			out = append(out, t)
		}
	}
	return out
}

// tagRegistry returns the known tag names — including ones created but not yet
// worn by any torrent, which is why the set cannot be derived from the torrents.
func (s *Server) tagRegistry() []string {
	if st := durable(); st != nil {
		names, err := st.TagNames()
		if err != nil {
			slog.Error("tags: reading registry failed", "err", err)
			return nil
		}
		return names
	}
	return tagstore.LoadRegistry(s.config.Daemon.DataDir)
}

// registerTags adds names to the persisted tag registry (known tags).
func (s *Server) registerTags(names []string) {
	if len(names) == 0 {
		return
	}
	if st := durable(); st != nil {
		if err := st.AddTagNames(names); err != nil {
			slog.Error("tags: registering failed", "err", err)
		}
		return
	}
	reg := map[string]bool{}
	for _, t := range tagstore.LoadRegistry(s.config.Daemon.DataDir) {
		reg[t] = true
	}
	for _, t := range names {
		reg[t] = true
	}
	out := make([]string, 0, len(reg))
	for t := range reg {
		out = append(out, t)
	}
	sort.Strings(out)
	_ = tagstore.SaveRegistry(s.config.Daemon.DataDir, out)
}

// unregisterTags drops names from the registry.
func (s *Server) unregisterTags(names []string) {
	if st := durable(); st != nil {
		if err := st.DeleteTagNames(names); err != nil {
			slog.Error("tags: unregistering failed", "err", err)
		}
		return
	}
	drop := map[string]bool{}
	for _, t := range names {
		drop[t] = true
	}
	s.unregisterTags(names)
}

// qbitTorrentsTags returns all known tags (registry union assigned).
func (s *Server) qbitTorrentsTags(c *gin.Context) {
	set := map[string]bool{}
	for _, t := range s.tagRegistry() {
		set[t] = true
	}
	if s.hoardEngine != nil {
		for _, tags := range s.hoardEngine.GetAllTags() {
			for _, t := range tags {
				set[t] = true
			}
		}
	}
	out := make([]string, 0, len(set))
	for t := range set {
		out = append(out, t)
	}
	sort.Strings(out)
	c.JSON(http.StatusOK, out)
}

// qbitCreateTags registers new (possibly unassigned) tags.
func (s *Server) qbitCreateTags(c *gin.Context) {
	s.registerTags(splitTagsCSV(c.PostForm("tags")))
	c.String(http.StatusOK, "")
}

// qbitDeleteTags removes tags from the registry and from every torrent.
func (s *Server) qbitDeleteTags(c *gin.Context) {
	names := splitTagsCSV(c.PostForm("tags"))
	drop := map[string]bool{}
	for _, t := range names {
		drop[t] = true
	}
	var reg []string
	for _, t := range tagstore.LoadRegistry(s.config.Daemon.DataDir) {
		if !drop[t] {
			reg = append(reg, t)
		}
	}
	_ = tagstore.SaveRegistry(s.config.Daemon.DataDir, reg)
	if s.hoardEngine != nil && len(names) > 0 {
		for ih, tags := range s.hoardEngine.GetAllTags() {
			for _, t := range tags {
				if drop[t] {
					s.hoardEngine.RemoveTags(ih, names)
					break
				}
			}
		}
		s.persistTagsAll()
	}
	c.String(http.StatusOK, "")
}

// qbitAddTags adds tags to the given torrents (hoard; race is a follow-up).
func (s *Server) qbitAddTags(c *gin.Context) {
	tags := splitTagsCSV(c.PostForm("tags"))
	hashes := parseHashes(c.PostForm("hashes"))
	if len(tags) > 0 && s.hoardEngine != nil {
		for _, h := range hashes {
			if s.hoardEngine.HasTorrent(h) {
				s.hoardEngine.AddTags(h, tags)
			}
		}
		s.registerTags(tags)
		s.persistTagsFor(hashes...)
	}
	c.String(http.StatusOK, "")
}

// qbitRemoveTags removes tags from the given torrents (empty tags = clear all).
func (s *Server) qbitRemoveTags(c *gin.Context) {
	tags := splitTagsCSV(c.PostForm("tags"))
	hashes := parseHashes(c.PostForm("hashes"))
	if s.hoardEngine != nil {
		for _, h := range hashes {
			if s.hoardEngine.HasTorrent(h) {
				s.hoardEngine.RemoveTags(h, tags)
			}
		}
		s.persistTagsFor(hashes...)
	}
	c.String(http.StatusOK, "")
}

func getBool(m map[string]interface{}, key string) bool {
	if v, ok := m[key]; ok {
		if b, ok := v.(bool); ok {
			return b
		}
	}
	return false
}

func getStr(m map[string]interface{}, key string) string {
	if v, ok := m[key]; ok {
		if s, ok := v.(string); ok {
			return s
		}
	}
	return ""
}

func getInt64(m map[string]interface{}, key string) int64 {
	if v, ok := m[key]; ok {
		switch n := v.(type) {
		case int64:
			return n
		case int:
			return int64(n)
		case float64:
			return int64(n)
		case int32:
			return int64(n)
		case uint64:
			return int64(n)
		}
	}
	return 0
}

func getFloat(m map[string]interface{}, key string) float64 {
	if v, ok := m[key]; ok {
		switch n := v.(type) {
		case float64:
			return n
		case float32:
			return float64(n)
		case int64:
			return float64(n)
		case int:
			return float64(n)
		}
	}
	return 0
}

func getRatio(m map[string]interface{}) float64 {
	ul := getInt64(m, "total_uploaded")
	dl := getInt64(m, "total_downloaded")
	if dl == 0 {
		return 0
	}
	return math.Round(float64(ul)/float64(dl)*100) / 100
}

func getETA(m map[string]interface{}) int64 {
	dlRate := getInt64(m, "download_rate")
	totalSize := getInt64(m, "total_size")
	downloaded := getInt64(m, "total_downloaded")
	if dlRate <= 0 || totalSize <= 0 {
		return 8640000
	}
	remaining := totalSize - downloaded
	if remaining <= 0 {
		return 0
	}
	return remaining / dlRate
}

// compareValues provides a generic less-than comparison for sorting.
func compareValues(a, b interface{}) bool {
	switch va := a.(type) {
	case string:
		if vb, ok := b.(string); ok {
			return va < vb
		}
	case int64:
		if vb, ok := b.(int64); ok {
			return va < vb
		}
	case int:
		if vb, ok := b.(int); ok {
			return va < vb
		}
	case float64:
		if vb, ok := b.(float64); ok {
			return va < vb
		}
	}
	return fmt.Sprintf("%v", a) < fmt.Sprintf("%v", b)
}

// qbitTorrentsRecheck forces a data re-hash (recheck) of the given torrents.
// Maps to the native verify path, which is hoard-only: the race engine has no
// picker to record per-piece verification. "all" is intentionally NOT honoured
// -- a mass recheck over the whole hoard (100k+) would be a disk storm; callers
// (autobrr/*arr) pass explicit hashes.
func (s *Server) qbitTorrentsRecheck(c *gin.Context) {
	for _, hash := range parseHashes(c.PostForm("hashes")) {
		hash = strings.ToLower(hash)
		if hash == "" || hash == "all" {
			continue
		}
		if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
			s.hoardEngine.VerifyTorrent(hash)
			continue
		}
		if ra, mode, ok := s.findRemoteOwner(hash); ok {
			ra.anyClient().ActionRouted(mode, "verify", hash, false, "", "")
		}
	}
	c.String(http.StatusOK, "")
}
