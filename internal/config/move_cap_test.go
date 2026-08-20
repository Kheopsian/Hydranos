package config

import "testing"

func TestMoveBytesPerSecondUnsetUsesDefault(t *testing.T) {
	var d DaemonConfig
	want := int64(DefaultMoveMaxMBPerSec) * 1024 * 1024
	if got := d.MoveBytesPerSecond(); got != want {
		t.Fatalf("unset cap = %d, want the default %d", got, want)
	}
}

func TestMoveBytesPerSecondExplicitZeroMeansUncapped(t *testing.T) {
	// The distinction the pointer exists for: an operator who writes
	// move_max_mb_per_sec = 0 is asking for no limit, not for the default.
	zero := 0
	d := DaemonConfig{MoveMaxMBPerSec: &zero}
	if got := d.MoveBytesPerSecond(); got != 0 {
		t.Fatalf("explicit zero = %d, want 0 (uncapped)", got)
	}
}

func TestMoveBytesPerSecondHonoursAValue(t *testing.T) {
	v := 50
	d := DaemonConfig{MoveMaxMBPerSec: &v}
	if got := d.MoveBytesPerSecond(); got != 50*1024*1024 {
		t.Fatalf("cap = %d, want %d", got, 50*1024*1024)
	}
}

func TestMoveBytesPerSecondNegativeIsTreatedAsUncapped(t *testing.T) {
	v := -1
	d := DaemonConfig{MoveMaxMBPerSec: &v}
	if got := d.MoveBytesPerSecond(); got != 0 {
		t.Fatalf("negative cap = %d, want 0", got)
	}
}
