package api

import (
	"archive/zip"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync/atomic"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/engine"
)

// Importing from Transmission means reading its config folder: it has no way to
// hand over a .torrent (see transmission.go). The folder can be one Hydra
// already sees, or a zip the browser uploads for an install on another machine.

type transmissionReq struct {
	Dir                string            `json:"dir"`
	PathMap            map[string]string `json:"path_map"`
	CategoriesFromDirs bool              `json:"categories_from_dirs"`
	ImportLabels       bool              `json:"import_labels"`
	// StartStopped imports everything stopped, whatever Transmission said, so
	// a trial run announces nothing until the user starts the torrents. Omitted
	// means true, same as the qBit import: land quiet, let the user check the
	// paths, then start.
	StartStopped *bool `json:"start_stopped"`
}

// startStopped reports whether the import should land everything paused.
// A missing field means yes.
func (r transmissionReq) startStopped() bool { return r.StartStopped == nil || *r.StartStopped }

// categoryFromDest turns a destination folder into a category name: the last
// path element, which is what people actually organise by. Empty for a bare
// root, where "imported" is a better answer than "".
func categoryFromDest(dest string) string {
	dest = strings.TrimRight(strings.TrimSpace(dest), "/")
	if dest == "" || dest == "/" {
		return ""
	}
	base := filepath.Base(dest)
	if base == "/" || base == "." {
		return ""
	}
	return base
}

// plan is what an import would do, computed once and reused by the preview and
// the run so the two can never disagree.
type transmissionPlan struct {
	Torrents   []transmissionTorrent
	Problems   []string
	Categories map[string]string // name -> save path
	Complete   int
	Stopped    int
	Carried    int64
	CarriedDL  int64
}

func (s *Server) planTransmission(req transmissionReq) (*transmissionPlan, error) {
	list, problems, err := scanTransmissionDir(req.Dir)
	if err != nil {
		return nil, err
	}
	p := &transmissionPlan{Torrents: list, Problems: problems, Categories: map[string]string{}}
	for _, t := range list {
		complete := t.Resume.DoneDate > 0
		if complete {
			p.Complete++
		}
		if t.Resume.Paused {
			p.Stopped++
		}
		p.Carried += t.Resume.Uploaded
		p.CarriedDL += t.Resume.Downloaded
		name := "imported"
		if req.CategoriesFromDirs {
			if n := categoryFromDest(t.Resume.Destination); n != "" {
				name = n
			}
		}
		sp := remapPath(t.savePathFor(complete), req.PathMap)
		if _, seen := p.Categories[name]; !seen {
			p.Categories[name] = sp
		}
	}
	return p, nil
}

// handleTransmissionPreview reports what would be imported. Reads only.
func (s *Server) handleTransmissionPreview(c *gin.Context) {
	var req transmissionReq
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	p, err := s.planTransmission(req)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	cats := []gin.H{}
	for name, sp := range p.Categories {
		cats = append(cats, gin.H{"name": name, "save_path": sp})
	}
	sort.Slice(cats, func(i, j int) bool { return cats[i]["name"].(string) < cats[j]["name"].(string) })

	sample := []gin.H{}
	prefixes := map[string]struct{}{}
	noResume := 0
	for i, t := range p.Torrents {
		if !t.HasResume {
			noResume++
		}
		if t.Resume.Destination != "" {
			prefixes[t.Resume.Destination] = struct{}{}
		}
		if i < 12 {
			sample = append(sample, gin.H{
				"name": t.Meta.Name, "save_path": t.Resume.Destination,
				"stopped": t.Resume.Paused, "labels": t.Resume.Labels,
			})
		}
	}
	prefixList := []string{}
	for k := range prefixes {
		prefixList = append(prefixList, k)
	}
	sort.Strings(prefixList)

	c.JSON(http.StatusOK, gin.H{
		"total":                  len(p.Torrents),
		"completed":              p.Complete,
		"incomplete":             len(p.Torrents) - p.Complete,
		"stopped":                p.Stopped,
		"without_resume":         noResume,
		"carried_uploaded_bytes": p.Carried,
		"categories":             cats,
		"path_prefixes":          prefixList,
		"problems":               p.Problems,
		"sample":                 sample,
	})
}

// handleTransmissionUpload takes a zip of the Transmission config folder for an
// install Hydra cannot see, unpacks it, and answers with the directory to pass
// back as "dir" -- so the run path stays identical for both sources.
func (s *Server) handleTransmissionUpload(c *gin.Context) {
	file, _, err := c.Request.FormFile("file")
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "no zip in request"})
		return
	}
	defer file.Close()

	dest := filepath.Join(s.config.Daemon.DataDir, "uploads",
		fmt.Sprintf("transmission-%d", time.Now().Unix()))
	if err := os.MkdirAll(dest, 0o755); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	tmpZip := dest + ".zip"
	out, err := os.Create(tmpZip)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if _, err := io.Copy(out, file); err != nil {
		out.Close()
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	out.Close()
	defer os.Remove(tmpZip)

	zr, err := zip.OpenReader(tmpZip)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "not a readable zip: " + err.Error()})
		return
	}
	defer zr.Close()

	extracted := 0
	for _, f := range zr.File {
		name := strings.ToLower(f.Name)
		if !strings.HasSuffix(name, ".torrent") && !strings.HasSuffix(name, ".resume") {
			continue
		}
		// Keep only the last two path elements (torrents/x.torrent), and never
		// let an archive entry escape the destination.
		rel := filepath.Join("torrents", filepath.Base(f.Name))
		if strings.HasSuffix(name, ".resume") {
			rel = filepath.Join("resume", filepath.Base(f.Name))
		}
		target := filepath.Join(dest, rel)
		if !strings.HasPrefix(target, filepath.Clean(dest)+string(os.PathSeparator)) {
			continue
		}
		if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
			continue
		}
		rc, err := f.Open()
		if err != nil {
			continue
		}
		w, err := os.Create(target)
		if err != nil {
			rc.Close()
			continue
		}
		if _, err := io.Copy(w, rc); err == nil {
			extracted++
		}
		w.Close()
		rc.Close()
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok", "dir": dest, "files": extracted})
}

// handleTransmissionStart runs the import in the background.
func (s *Server) handleTransmissionStart(c *gin.Context) {
	var req transmissionReq
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	importMu.Lock()
	if importCurrent != nil && !importCurrent.get().Finished {
		importMu.Unlock()
		c.JSON(http.StatusConflict, gin.H{"error": "an import is already running"})
		return
	}
	job := newImportJob(fmt.Sprintf("transmission-%d", time.Now().Unix()))
	importCurrent = job
	importMu.Unlock()

	go s.runTransmissionImport(job, req)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "job_id": job.get().JobID})
}

func (s *Server) runTransmissionImport(job *importJob, req transmissionReq) {
	defer job.finish()
	fail := func(msg string) {
		slog.Error("transmission import failed", "error", msg)
		job.update(func(sn *importSnapshot) { sn.Phase = "error"; sn.Error = msg })
	}
	if s.hoardEngine == nil {
		fail("hoard engine not available")
		return
	}
	p, err := s.planTransmission(req)
	if err != nil {
		fail(err.Error())
		return
	}
	slog.Info("transmission import: starting", "dir", req.Dir, "torrents", len(p.Torrents))
	job.update(func(sn *importSnapshot) { sn.Phase = "categories"; sn.Total = len(p.Torrents) })
	for _, pb := range p.Problems {
		job.update(func(sn *importSnapshot) { appendErr(sn, pb) })
	}

	// Categories first, so no torrent transiently lands in the race default.
	existing := loadCategories(s.config.Daemon.DataDir)
	byName := map[string]bool{}
	for _, ec := range existing {
		byName[strings.ToLower(ec.Name)] = true
	}
	changed := false
	for name, sp := range p.Categories {
		if byName[strings.ToLower(name)] {
			continue // never clobber one the user already has
		}
		existing = append(existing, category{Name: name, SavePath: sp, Mode: "hoard"})
		byName[strings.ToLower(name)] = true
		changed = true
	}
	if changed {
		if err := saveCategories(s.config.Daemon.DataDir, existing); err != nil {
			fail("save categories: " + err.Error())
			return
		}
	}

	job.update(func(sn *importSnapshot) { sn.Phase = "torrents" })
	var carried, carriedDL, earliest int64
	for _, t := range p.Torrents {
		name := t.Meta.Name
		if name == "" {
			name = t.Meta.InfoHash[:12]
		}
		complete := t.Resume.DoneDate > 0
		sp := remapPath(t.savePathFor(complete), req.PathMap)
		if sp == "" {
			job.update(func(sn *importSnapshot) {
				sn.Failed++
				sn.Done++
				appendErr(sn, name+": no destination in the resume file")
			})
			continue
		}
		cat := "imported"
		if req.CategoriesFromDirs {
			if n := categoryFromDest(t.Resume.Destination); n != "" {
				cat = n
			}
		}

		var ih string
		var addErr error
		// A torrent Transmission had finished is adopted, never re-downloaded:
		// the add checks every declared file against the disk and is refused
		// with the path it looked at when they are not there. A path map that
		// is one level off then costs an error line, not a re-fetch of the
		// whole library.
		if complete {
			ih, addErr = s.hoardEngine.AddTorrentOpts(t.TorrentPath, sp, cat,
				engine.AddOptions{SkipRecheck: true})
		} else {
			ih, addErr = s.hoardEngine.AddTorrent(t.TorrentPath, sp, cat)
		}
		if addErr != nil {
			el := strings.ToLower(addErr.Error())
			if strings.Contains(el, "already") || strings.Contains(el, "duplicate") || strings.Contains(el, "exists") {
				job.update(func(sn *importSnapshot) { sn.Skipped++; sn.Done++ })
				continue
			}
			job.update(func(sn *importSnapshot) {
				sn.Failed++
				sn.Done++
				appendErr(sn, name+": add: "+addErr.Error())
			})
			continue
		}

		if t.Resume.AddedDate > 0 {
			s.hoardEngine.SetAddedTime(ih, time.Unix(t.Resume.AddedDate, 0))
			if earliest == 0 || t.Resume.AddedDate < earliest {
				earliest = t.Resume.AddedDate
			}
		}
		// Same for the completion date, else every imported seed reads as
		// "finished at import time".
		if t.Resume.DoneDate > 0 {
			s.hoardEngine.SetCompletedTime(ih, time.Unix(t.Resume.DoneDate, 0))
		}
		// Transmission labels are multi-valued and carry no path, so they are
		// our tags -- not our categories, which own a save path.
		if req.ImportLabels && len(t.Resume.Labels) > 0 {
			if err := s.hoardEngine.SetTags(ih, t.Resume.Labels); err != nil {
				slog.Debug("transmission import: tags", "name", name, "error", err)
			}
		}
		// A torrent paused in Transmission stays stopped here. Set right after
		// the add so the bootstrap announce finds the intent and stays quiet.
		if req.startStopped() || t.Resume.Paused {
			if err := s.hoardEngine.SetUserPaused(ih, true); err == nil {
				persistPaused([]string{ih}, true)
			}
		}
		carried += t.Resume.Uploaded
		carriedDL += t.Resume.Downloaded
		job.update(func(sn *importSnapshot) {
			if complete {
				sn.Seeded++
			} else {
				sn.Downloading++
			}
			sn.Done++
		})
	}

	prov, _ := loadProvenance(s.config.Daemon.DataDir)
	prov.SourceClient = "Transmission"
	if earliest > 0 && (prov.SourceDate == 0 || earliest < prov.SourceDate) {
		prov.SourceDate = earliest
	}
	if prov.SourceDate == 0 {
		prov.SourceDate = time.Now().Unix()
	}
	prov.CarriedUploadedBytes += carried
	final := job.get()
	prov.ImportedCount += final.Seeded + final.Downloading
	_ = saveProvenance(s.config.Daemon.DataDir, prov)

	atomic.AddInt64(&baselineUploaded, carried)
	atomic.AddInt64(&baselineDownloaded, carriedDL)
	saveBaseline()
	if s.saveStateFn != nil {
		s.saveStateFn()
	}
	job.update(func(sn *importSnapshot) { sn.Phase = "done" })
	slog.Info("transmission import: done",
		"seeded", final.Seeded, "downloading", final.Downloading,
		"skipped", final.Skipped, "failed", final.Failed)
}
