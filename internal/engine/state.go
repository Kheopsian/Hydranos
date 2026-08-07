package engine

// Halted torrents come in two flavours and the difference is the whole point of
// this file: "stopped" means a human stopped it and nothing automatic may undo
// that, "queued" means one of our schedulers is holding it and it will come
// back on its own. The engine only reports "halted", so the intent flag is what
// tells them apart -- which is why the state string is rewritten at every site
// that writes the flag.
//
// The vocabulary matches qBittorrent 5 (stop/start, stoppedUP/stoppedDL), so
// automation written against qBit reads it without a translation table.

// StateStopped is a torrent halted by the user.
const StateStopped = "stopped"

// StateQueued is a torrent halted by a scheduler.
const StateQueued = "queued"

// haltedState maps a halted torrent to the right word.
func haltedState(userStopped bool) string {
	if userStopped {
		return StateStopped
	}
	return StateQueued
}

// DeriveState turns an engine-reported state into the user-facing one. Only the
// halted states are ambiguous; everything else passes through untouched.
func DeriveState(raw string, userStopped bool) string {
	switch raw {
	case "paused", StateStopped, StateQueued, "":
		return haltedState(userStopped)
	}
	if userStopped {
		// The intent is authoritative: the engine may still be reporting the
		// state it had a tick before the stop landed.
		return StateStopped
	}
	return raw
}
