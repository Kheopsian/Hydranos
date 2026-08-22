package api

import (
	"reflect"
	"testing"
)

// qBittorrent is not consistent with itself: addTrackers posts newline
// separated URLs, removeTrackers posts them pipe separated. Accepting only one
// form makes the other route silently drop every URL but the first.
func TestSplitTrackerURLsAcceptsBothSeparators(t *testing.T) {
	tests := []struct {
		name string
		in   string
		want []string
	}{
		{"newlines", "http://a.invalid/announce\nhttp://b.invalid/announce",
			[]string{"http://a.invalid/announce", "http://b.invalid/announce"}},
		{"pipes", "http://a.invalid/announce|http://b.invalid/announce",
			[]string{"http://a.invalid/announce", "http://b.invalid/announce"}},
		{"crlf", "http://a.invalid/announce\r\nhttp://b.invalid/announce",
			[]string{"http://a.invalid/announce", "http://b.invalid/announce"}},
		{"single", "http://a.invalid/announce", []string{"http://a.invalid/announce"}},
		{"blank entries dropped", "\n\nhttp://a.invalid/announce\n\n",
			[]string{"http://a.invalid/announce"}},
		{"empty", "", []string{}},
		{"whitespace only", "  \n ", []string{}},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := splitTrackerURLs(tc.in); !reflect.DeepEqual(got, tc.want) {
				t.Errorf("got %q, want %q", got, tc.want)
			}
		})
	}
}
