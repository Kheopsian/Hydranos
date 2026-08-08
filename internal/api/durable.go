package api

// The small settings documents -- the category list, the import provenance --
// live in the store next to the torrents they describe, instead of in JSON
// files beside the database. The TOML keeps the daemon's own configuration.
//
// A front-only node has no store, so both helpers report "nothing here" and the
// callers fall back to the files, exactly like the counters above.

// metaDoc reads a settings document; "" when absent or when there is no store.
func metaDoc(key string) string {
	s := durable()
	if s == nil {
		return ""
	}
	v, err := s.GetMeta(key)
	if err != nil {
		return ""
	}
	return v
}

// setMetaDoc writes a settings document. Reports whether the store took it.
func setMetaDoc(key, value string) bool {
	s := durable()
	if s == nil {
		return false
	}
	return s.SetMeta(key, value) == nil
}
