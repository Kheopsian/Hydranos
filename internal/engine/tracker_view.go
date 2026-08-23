package engine

import (
	"encoding/json"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Tracker rows for the detail panel.
//
// Typhon reports one tracker state per torrent and fills it from its OWN
// announce loop -- which is disabled in every Hydra deployment, because the Go
// side owns announcing. Its `last_error` is therefore empty, which it renders
// as the string "Success", and the panel painted a green OK for a tracker that
// had never been contacted at all. A torrent that announces to nobody looked
// perfectly healthy on the one screen an operator opens to check exactly that.
//
// So the rule here is: an endpoint says what WE observed, or it says nothing.

// TrackerRows builds the detail view's tracker list: the engine's tracker URLs,
// with each endpoint overwritten by what our announce loop actually saw.
func TrackerRows(trackers []ltclient.TrackerInfo, obsByURL map[string]TrackerObservation) []map[string]interface{} {
	rows := make([]map[string]interface{}, 0, len(trackers))
	for _, t := range trackers {
		var endpoints []map[string]interface{}
		if len(t.Endpoints) > 0 {
			_ = json.Unmarshal(t.Endpoints, &endpoints)
		}
		if len(endpoints) == 0 {
			endpoints = []map[string]interface{}{{}}
		}
		obs, observed := obsByURL[t.URL]
		for i := range endpoints {
			applyTrackerObs(endpoints[i], obs, observed)
		}
		rows = append(rows, map[string]interface{}{
			"url":       t.URL,
			"tier":      t.Tier,
			"verified":  observed && obs.OK,
			"endpoints": endpoints,
		})
	}
	return rows
}

// applyTrackerObs writes one observation onto one endpoint, or clears the
// engine's placeholder when there is nothing to write.
func applyTrackerObs(ep map[string]interface{}, obs TrackerObservation, observed bool) {
	if !observed {
		// Not "Success", and not an error either -- we have not tried yet, or
		// the try is still in flight. The UI renders the empty state as a dash.
		ep["last_error"] = ""
		ep["message"] = ""
		ep["last_announce"] = -1
		ep["next_announce"] = -1
		return
	}
	if obs.OK {
		ep["last_error"] = "Success"
		ep["message"] = ""
		ep["scrape_complete"] = obs.Seeds
		ep["scrape_incomplete"] = obs.Leechers
	} else {
		ep["last_error"] = obs.ErrorMsg
		ep["message"] = obs.ErrorMsg
	}
	if obs.NextAt.IsZero() {
		ep["next_announce"] = -1
	} else {
		secs := int64(time.Until(obs.NextAt).Seconds())
		if secs < 0 {
			secs = 0
		}
		ep["next_announce"] = secs
	}
	if obs.LastAt.IsZero() {
		ep["last_announce"] = -1
	} else {
		ep["last_announce"] = int64(time.Since(obs.LastAt).Seconds())
	}
}

// TrackerRowsWithObs is TrackerRows for a caller that has an info hash rather
// than an observation map -- every in-process caller, since the observations
// are recorded by whichever engine did the announcing.
func TrackerRowsWithObs(infoHash string, trackers []ltclient.TrackerInfo) []map[string]interface{} {
	return TrackerRows(trackers, trackerObsFor(infoHash))
}

// EncodeTrackerRows folds the rows back into the wire shape an agent replies
// with, so a front asking a remote engine for a torrent's trackers gets that
// agent's observations instead of the engine placeholder it used to pass
// through untouched.
func EncodeTrackerRows(trackers []ltclient.TrackerInfo, rows []map[string]interface{}) []ltclient.TrackerInfo {
	out := make([]ltclient.TrackerInfo, 0, len(trackers))
	for i, t := range trackers {
		if i >= len(rows) {
			break
		}
		if eps, err := json.Marshal(rows[i]["endpoints"]); err == nil {
			t.Endpoints = eps
		}
		if v, ok := rows[i]["verified"].(bool); ok {
			t.Verified = v
		}
		out = append(out, t)
	}
	return out
}
