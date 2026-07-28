package config

import "testing"

func TestResolveEnginesLegacy(t *testing.T) {
	c := &HydraConfig{
		Race:  SessionConfig{ListenPort: 16171},
		Hoard: SessionConfig{ListenPort: 16172},
	}
	engs, err := c.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	if len(engs) != 2 {
		t.Fatalf("want 2 legacy engines, got %d", len(engs))
	}
	if engs[0].ID != "race" || engs[0].Role != "race" || engs[0].ListenPort != 16171 {
		t.Fatalf("race engine wrong: %+v", engs[0])
	}
	if engs[1].ID != "hoard" || engs[1].Role != "hoard" || engs[1].ListenPort != 16172 {
		t.Fatalf("hoard engine wrong: %+v", engs[1])
	}
}

func TestResolveEnginesExplicit(t *testing.T) {
	c := &HydraConfig{
		Race: SessionConfig{ListenPort: 1}, // ignored when Engines present
		Engines: []EngineConfig{
			{ID: "race", Role: "race", SessionConfig: SessionConfig{ListenPort: 16171}},
			{ID: "hoard", Role: "hoard", SessionConfig: SessionConfig{ListenPort: 16172}},
			{ID: "hoard2", Role: "hoard", SessionConfig: SessionConfig{ListenPort: 16182}},
		},
	}
	engs, err := c.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	if len(engs) != 3 {
		t.Fatalf("want 3 engines, got %d", len(engs))
	}
	if engs[2].ID != "hoard2" || engs[2].Role != "hoard" {
		t.Fatalf("hoard2 wrong: %+v", engs[2])
	}
}

func TestResolveEnginesValidation(t *testing.T) {
	cases := []struct {
		name string
		engs []EngineConfig
	}{
		{"dup id", []EngineConfig{
			{ID: "h", Role: "hoard", SessionConfig: SessionConfig{ListenPort: 1}},
			{ID: "h", Role: "hoard", SessionConfig: SessionConfig{ListenPort: 2}},
		}},
		{"bad role", []EngineConfig{{ID: "x", Role: "seed", SessionConfig: SessionConfig{ListenPort: 1}}}},
		{"dup port", []EngineConfig{
			{ID: "a", Role: "race", SessionConfig: SessionConfig{ListenPort: 9}},
			{ID: "b", Role: "hoard", SessionConfig: SessionConfig{ListenPort: 9}},
		}},
		{"empty id", []EngineConfig{{ID: "", Role: "race", SessionConfig: SessionConfig{ListenPort: 1}}}},
	}
	for _, tc := range cases {
		c := &HydraConfig{Engines: tc.engs}
		if _, err := c.ResolveEngines(); err == nil {
			t.Fatalf("%s: expected validation error", tc.name)
		}
	}
}
