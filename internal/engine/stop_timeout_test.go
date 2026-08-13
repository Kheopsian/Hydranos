package engine

import (
	"testing"
	"time"
)

func TestStopTimeout(t *testing.T) {
	cases := []struct {
		name string
		env  string
		want time.Duration
	}{
		{"unset falls back to the default", "", defaultStopTimeout},
		{"blank falls back to the default", "   ", defaultStopTimeout},
		{"a Go duration is honoured", "45s", 45 * time.Second},
		{"minutes work too", "2m", 2 * time.Minute},
		{"a bare number reads as seconds", "45", 45 * time.Second},
		{"garbage falls back rather than leaving it unbounded", "soon", defaultStopTimeout},
		{"zero would kill the engine instantly", "0", defaultStopTimeout},
		{"negative would too", "-5s", defaultStopTimeout},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Setenv("HYDRA_STOP_TIMEOUT", tc.env)
			if got := StopTimeout(); got != tc.want {
				t.Fatalf("StopTimeout() with %q = %v, want %v", tc.env, got, tc.want)
			}
		})
	}
}
