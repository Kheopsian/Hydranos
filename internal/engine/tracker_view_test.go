package engine

import (
	"encoding/json"
	"testing"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

func endpointOf(t *testing.T, row map[string]interface{}) map[string]interface{} {
	t.Helper()
	eps, ok := row["endpoints"].([]map[string]interface{})
	if !ok || len(eps) == 0 {
		t.Fatalf("row has no endpoints: %+v", row)
	}
	return eps[0]
}

// The engine reports "Success" for a tracker it has never contacted, because
// its own announce loop is off and its last_error is therefore empty. Passing
// that through painted a green OK on a torrent that was announcing to nobody --
// which is the one thing the panel exists to show.
func TestTrackerRowsDoesNotInventSuccess(t *testing.T) {
	trackers := []ltclient.TrackerInfo{{
		URL:       "http://t.invalid:6969/announce",
		Endpoints: json.RawMessage(`[{"last_error":"Success","message":"","last_announce":-1,"next_announce":-1}]`),
	}}

	row := TrackerRows(trackers, nil)[0]
	ep := endpointOf(t, row)
	if ep["last_error"] != "" {
		t.Errorf("last_error = %q, want empty: nothing has been observed", ep["last_error"])
	}
	if row["verified"] != false {
		t.Errorf("verified = %v, want false", row["verified"])
	}
	if ep["last_announce"] != -1 {
		t.Errorf("last_announce = %v, want -1 (never)", ep["last_announce"])
	}
}

func TestTrackerRowsReportsTheObservedFailure(t *testing.T) {
	trackers := []ltclient.TrackerInfo{{
		URL:       "http://t.invalid:6969/announce",
		Endpoints: json.RawMessage(`[{"last_error":"Success"}]`),
	}}
	obs := map[string]TrackerObservation{
		"http://t.invalid:6969/announce": {ErrorMsg: "dial tcp: connection refused"},
	}

	row := TrackerRows(trackers, obs)[0]
	ep := endpointOf(t, row)
	if ep["last_error"] != "dial tcp: connection refused" {
		t.Errorf("last_error = %q, want the transport error", ep["last_error"])
	}
	if row["verified"] != false {
		t.Errorf("verified = %v, want false", row["verified"])
	}
}

func TestTrackerRowsReportsASuccessfulAnnounce(t *testing.T) {
	trackers := []ltclient.TrackerInfo{{URL: "http://t.invalid:6969/announce"}}
	obs := map[string]TrackerObservation{
		"http://t.invalid:6969/announce": {
			OK: true, Seeds: 3, Leechers: 7,
			LastAt: time.Now().Add(-30 * time.Second),
			NextAt: time.Now().Add(20 * time.Minute),
		},
	}

	row := TrackerRows(trackers, obs)[0]
	ep := endpointOf(t, row)
	if ep["last_error"] != "Success" {
		t.Errorf("last_error = %q, want Success", ep["last_error"])
	}
	if ep["scrape_complete"] != 3 || ep["scrape_incomplete"] != 7 {
		t.Errorf("scrape = %v/%v, want 3/7", ep["scrape_complete"], ep["scrape_incomplete"])
	}
	if secs, _ := ep["last_announce"].(int64); secs < 29 || secs > 31 {
		t.Errorf("last_announce = %v, want ~30s ago", ep["last_announce"])
	}
	if row["verified"] != true {
		t.Errorf("verified = %v, want true", row["verified"])
	}
}

// The agent replies in the engine's own wire shape, so the corrected endpoints
// have to survive the trip back into it.
func TestEncodeTrackerRowsCarriesTheObservationsBack(t *testing.T) {
	trackers := []ltclient.TrackerInfo{{
		URL:       "http://t.invalid:6969/announce",
		Endpoints: json.RawMessage(`[{"last_error":"Success"}]`),
	}}
	obs := map[string]TrackerObservation{
		"http://t.invalid:6969/announce": {ErrorMsg: "no such host"},
	}

	out := EncodeTrackerRows(trackers, TrackerRows(trackers, obs))
	if len(out) != 1 {
		t.Fatalf("got %d trackers, want 1", len(out))
	}
	var eps []map[string]interface{}
	if err := json.Unmarshal(out[0].Endpoints, &eps); err != nil {
		t.Fatalf("endpoints did not survive: %v", err)
	}
	if eps[0]["last_error"] != "no such host" {
		t.Errorf("last_error = %q, want the observed error", eps[0]["last_error"])
	}
	if out[0].Verified {
		t.Error("verified stayed true for a tracker we cannot reach")
	}
}

// One tracker answering must not erase what we know about the others: the race
// loop announces them one at a time.
func TestRecordTrackerObsKeepsTheOtherTrackers(t *testing.T) {
	const ih = "635a6385719b0a9d0abc41d27b3aca0b449b5cb7"
	defer forgetTrackerObs(ih)

	recordTrackerObs(ih, "http://a.invalid/announce", TrackerObservation{OK: true, Seeds: 1})
	recordTrackerObs(ih, "udp://a.invalid/announce", TrackerObservation{ErrorMsg: "timeout"})

	got := trackerObsFor(ih)
	if len(got) != 2 {
		t.Fatalf("got %d observations, want both trackers: %+v", len(got), got)
	}
	if !got["http://a.invalid/announce"].OK {
		t.Error("the http tracker's success was lost")
	}
	if got["udp://a.invalid/announce"].ErrorMsg != "timeout" {
		t.Error("the udp tracker's failure was lost")
	}
}
