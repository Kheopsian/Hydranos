package engine

import (
	"fmt"
	"reflect"
	"testing"
)

// bencode helpers, kept local so the fixtures read like the bytes on disk.
func bstr(s string) string { return fmt.Sprintf("%d:%s", len(s), s) }
func blist(v ...string) string {
	out := "l"
	for _, x := range v {
		out += x
	}
	return out + "e"
}

func TestTrackerURLsReadsAnnounceList(t *testing.T) {
	tests := []struct {
		name string
		tor  string
		want []string
	}{
		{
			name: "announce only",
			tor:  "d" + bstr("announce") + bstr("http://a.invalid/announce") + "e",
			want: []string{"http://a.invalid/announce"},
		},
		{
			// The whole point: a cross-seeded torrent lists several trackers
			// and every one of them has to see us.
			name: "announce plus tiers",
			tor: "d" + bstr("announce") + bstr("http://a.invalid/announce") +
				bstr("announce-list") + blist(
				blist(bstr("http://a.invalid/announce")),
				blist(bstr("http://b.invalid/announce"), bstr("http://c.invalid/announce")),
			) + "e",
			want: []string{
				"http://a.invalid/announce",
				"http://b.invalid/announce",
				"http://c.invalid/announce",
			},
		},
		{
			// BEP 12 allows announce-list alone. Such a torrent used to get no
			// announce at all.
			name: "announce-list only",
			tor: "d" + bstr("announce-list") + blist(
				blist(bstr("http://b.invalid/announce")),
			) + "e",
			want: []string{"http://b.invalid/announce"},
		},
		{
			// The tracker offers three endpoints and we have a client for two
			// of them. Announcing to tcp:// fails on every pass of the loop and
			// counts against the breaker for a host that is answering fine.
			name: "unsupported scheme dropped",
			tor: "d" + bstr("announce") + bstr("http://a.invalid:6969/announce") +
				bstr("announce-list") + blist(
				blist(bstr("http://a.invalid:6969/announce")),
				blist(bstr("udp://a.invalid:6969/announce")),
				blist(bstr("tcp://a.invalid:6970")),
			) + "e",
			want: []string{
				"http://a.invalid:6969/announce",
				"udp://a.invalid:6969/announce",
			},
		},
		{
			name: "no tracker",
			tor:  "d" + bstr("comment") + bstr("nothing here") + "e",
			want: nil,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got := trackerURLsFromTorrentFile([]byte(tc.tor))
			if !reflect.DeepEqual(got, tc.want) {
				t.Errorf("got %q, want %q", got, tc.want)
			}
		})
	}
}

func TestTrackerURLsDeduplicates(t *testing.T) {
	// announce almost always repeats as the first tier; announcing twice to the
	// same URL doubles our announce rate against that tracker for nothing.
	tor := "d" + bstr("announce") + bstr("http://a.invalid/announce") +
		bstr("announce-list") + blist(
		blist(bstr("http://a.invalid/announce")),
		blist(bstr("http://a.invalid/announce")),
	) + "e"
	if got := trackerURLsFromTorrentFile([]byte(tor)); len(got) != 1 {
		t.Errorf("got %q, want the URL once", got)
	}
}
