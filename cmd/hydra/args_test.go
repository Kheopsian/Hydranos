package main

import "testing"

func TestConfigPathFromArgs(t *testing.T) {
	cases := []struct {
		name string
		args []string
		want string
	}{
		{"positional, the documented form", []string{"/etc/hydra/default.toml"}, "/etc/hydra/default.toml"},
		{"--config, the form everyone tries first", []string{"--config", "/etc/hydra/default.toml"}, "/etc/hydra/default.toml"},
		{"--config=path", []string{"--config=/etc/hydra/default.toml"}, "/etc/hydra/default.toml"},
		{"single dash is accepted too", []string{"-config", "/etc/hydra/default.toml"}, "/etc/hydra/default.toml"},
		{"-config=path", []string{"-config=/etc/hydra/default.toml"}, "/etc/hydra/default.toml"},
		{"nothing given falls through to the caller's default", nil, ""},
		{"a dangling --config is not a path", []string{"--config"}, ""},
		{"an unrelated flag is skipped, not taken as the path", []string{"-v", "/etc/hydra/default.toml"}, "/etc/hydra/default.toml"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := configPathFromArgs(tc.args); got != tc.want {
				t.Fatalf("configPathFromArgs(%q) = %q, want %q", tc.args, got, tc.want)
			}
		})
	}
}

func TestMisplacedSubcommand(t *testing.T) {
	// The case that cost a user an evening: the subcommand after --config used
	// to start the daemon and sit there.
	if got := misplacedSubcommand([]string{"/etc/hydra/default.toml", "reset-password", "hunter2"}); got != "reset-password" {
		t.Fatalf("expected reset-password to be reported as misplaced, got %q", got)
	}
	if got := misplacedSubcommand([]string{"hash-password"}); got != "hash-password" {
		t.Fatalf("expected hash-password to be reported as misplaced, got %q", got)
	}
	// A plain daemon invocation must not be mistaken for one.
	if got := misplacedSubcommand([]string{"/etc/hydra/default.toml"}); got != "" {
		t.Fatalf("a daemon invocation was flagged as a misplaced %q", got)
	}
}
