package store

// AllSeedTimes returns every non-zero cumulative seed time, keyed by info_hash.
//
// Read once at boot to re-seed the in-memory clock: the counter is accumulated
// in the process and only flushed to the store on the sync tick, so without
// this the whole catalogue would restart from zero at every restart -- which
// is exactly the failure the counter exists to fix.
//
// Deliberately a two-column scan and nothing else: the torrents table carries
// the .torrent blob on every row, so any query that opens a row reads the
// whole database file. This one is served by the columns alone.
func (s *Store) AllSeedTimes() (map[string]int64, error) {
	rows, err := s.db.Query(`SELECT info_hash, seeding_time FROM torrents WHERE seeding_time > 0`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string]int64{}
	for rows.Next() {
		var ih string
		var secs int64
		if err := rows.Scan(&ih, &secs); err != nil {
			return nil, err
		}
		out[ih] = secs
	}
	return out, rows.Err()
}
