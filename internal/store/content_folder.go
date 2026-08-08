package store

// contentFolderInt encodes the tri-state: nil (unknown, legacy layout) is -1,
// which keeps the column NOT NULL and keeps "never told" apart from "told no".
func contentFolderInt(cf *bool) int {
	if cf == nil {
		return -1
	}
	if *cf {
		return 1
	}
	return 0
}

// SetContentFolder records the content layout of one torrent. Used by the
// one-shot import of state.json, and by anything that changes the layout.
func (s *Store) SetContentFolder(infoHash string, cf *bool) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`UPDATE torrents SET content_folder = ? WHERE info_hash = ?`,
		contentFolderInt(cf), infoHash)
	return err
}

// ContentFolderUnknown counts rows still carrying the "never told" marker, so
// the boot log can say whether the one-shot has anything left to do.
func (s *Store) ContentFolderUnknown() (int, error) {
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM torrents WHERE content_folder < 0`).Scan(&n)
	return n, err
}
