package main

import (
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

func TestParseEngineSpec(t *testing.T) {
	got, err := parseEngineSpec("id=race-0,role=race,port=12314,ipv6=true")
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	want := agentBootEngine{ID: "race-0", Role: "race", ListenPort: 12314, EnableIPv6: true}
	if got != want {
		t.Fatalf("parsed %+v, want %+v", got, want)
	}
}

// A malformed spec must stop the boot rather than degrade to a default. An
// agent silently listening on the wrong port is a whole announce cycle of
// wrong data on every tracker before anyone notices.
func TestParseEngineSpecRejectsGarbage(t *testing.T) {
	for _, spec := range []string{
		"id=race-0,role=race,port=nope",
		"id=race-0,role=race,ipv6=maybe",
		"id=race-0,rolle=race",
		"id=race-0,role",
	} {
		if _, err := parseEngineSpec(spec); err == nil {
			t.Fatalf("spec %q was accepted", spec)
		}
	}
}

// The two env spellings and the flag all feed one parser, so a node written
// three ways boots identically.
func TestResolveAgentBootSourcesAgree(t *testing.T) {
	want := []agentBootEngine{{ID: "race-0", Role: "race", ListenPort: 12314, EnableIPv6: true}}
	cfg := config.DefaultConfig()

	fromFlag, src, err := resolveAgentBoot([]string{"id=race-0,role=race,port=12314,ipv6=true"}, cfg)
	if err != nil || src != "flag" {
		t.Fatalf("flag form: src=%q err=%v", src, err)
	}
	assertBootEquals(t, "flag", fromFlag, want)

	t.Setenv(envEngines, "id=race-0,role=race,port=12314,ipv6=true")
	fromEngines, src, err := resolveAgentBoot(nil, cfg)
	if err != nil || src != "env" {
		t.Fatalf("HYDRA_ENGINES form: src=%q err=%v", src, err)
	}
	assertBootEquals(t, envEngines, fromEngines, want)

	t.Setenv(envEngines, "")
	t.Setenv(envEngineID, "race-0")
	t.Setenv(envEngineRole, "race")
	t.Setenv(envEngineListenPort, "12314")
	t.Setenv(envEngineEnableIPv6, "true")
	fromSingle, src, err := resolveAgentBoot(nil, cfg)
	if err != nil || src != "env" {
		t.Fatalf("HYDRA_ENGINE_* form: src=%q err=%v", src, err)
	}
	assertBootEquals(t, envEngineID, fromSingle, want)
}

// Flag beats env beats file, the same order resolveAgentToken uses, so there
// is one precedence rule for the whole agent rather than one per setting.
func TestResolveAgentBootPrecedence(t *testing.T) {
	cfg := config.DefaultConfig()
	cfg.Engines = []config.EngineConfig{{ID: "from-file", Role: "race"}}
	t.Setenv(envEngineID, "from-env")
	t.Setenv(envEngineRole, "race")

	got, src, err := resolveAgentBoot([]string{"id=from-flag,role=race"}, cfg)
	if err != nil {
		t.Fatal(err)
	}
	if src != "flag" || got[0].ID != "from-flag" {
		t.Fatalf("the flag lost to another source: src=%q id=%q", src, got[0].ID)
	}

	got, src, err = resolveAgentBoot(nil, cfg)
	if err != nil {
		t.Fatal(err)
	}
	if src != "env" || got[0].ID != "from-env" {
		t.Fatalf("the environment lost to the file: src=%q id=%q", src, got[0].ID)
	}
}

func TestResolveAgentBootFallsBackToTheConfigFile(t *testing.T) {
	cfg := config.DefaultConfig()
	cfg.Engines = []config.EngineConfig{
		{ID: "hoard-0", Role: "hoard", SessionConfig: config.SessionConfig{ListenPort: 12313, EnableIPv6: true}},
	}
	got, src, err := resolveAgentBoot(nil, cfg)
	if err != nil {
		t.Fatal(err)
	}
	if src != "file" {
		t.Fatalf("source = %q, want file", src)
	}
	assertBootEquals(t, "file", got, []agentBootEngine{
		{ID: "hoard-0", Role: "hoard", ListenPort: 12313, EnableIPv6: true},
	})
}

func TestValidateAgentBoot(t *testing.T) {
	cases := []struct {
		name    string
		engines []agentBootEngine
		want    string
	}{
		{"nothing declared", nil, "no engine declared"},
		{"empty id", []agentBootEngine{{Role: "race"}}, "empty id"},
		{"bad role", []agentBootEngine{{ID: "a", Role: "seedbox"}}, "role must be"},
		{"duplicate id", []agentBootEngine{
			{ID: "a", Role: "race"}, {ID: "a", Role: "hoard"},
		}, "duplicate engine id"},
		{"duplicate port leaves one engine dead", []agentBootEngine{
			{ID: "a", Role: "race", ListenPort: 1234}, {ID: "b", Role: "hoard", ListenPort: 1234},
		}, "already used"},
		{"port out of range", []agentBootEngine{{ID: "a", Role: "race", ListenPort: 70000}}, "out of range"},
	}
	for _, c := range cases {
		err := validateAgentBoot(c.engines)
		if err == nil {
			t.Fatalf("%s: accepted", c.name)
		}
		if !strings.Contains(err.Error(), c.want) {
			t.Fatalf("%s: error %q does not mention %q", c.name, err, c.want)
		}
	}

	ok := []agentBootEngine{
		{ID: "race-0", Role: "race", ListenPort: 12314},
		{ID: "hoard-0", Role: "hoard", ListenPort: 12313},
	}
	if err := validateAgentBoot(ok); err != nil {
		t.Fatalf("a valid identity was rejected: %v", err)
	}
}

func assertBootEquals(t *testing.T, form string, got, want []agentBootEngine) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("%s: %d engines, want %d", form, len(got), len(want))
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("%s: engine %d = %+v, want %+v", form, i, got[i], want[i])
		}
	}
}
