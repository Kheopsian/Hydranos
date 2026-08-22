package main

import (
	"strings"
	"testing"
)

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

func TestLeftoverArgsMessage(t *testing.T) {
	// A plain daemon invocation leaves nothing behind.
	if got := leftoverArgsMessage(nil); got != "" {
		t.Fatalf("a clean invocation was reported as leftover args: %q", got)
	}

	// The compose case: the entrypoint already ran `hydra --config <cfg>`, so a
	// `command:` repeating the binary name leaves "hydra ..." positional and
	// flag.Parse drops --agent-only. Booting a monolith there is the bug.
	got := leftoverArgsMessage([]string{"hydra", "--config", "/config/default.toml", "--agent-only"})
	if got == "" {
		t.Fatal("the duplicated-entrypoint form was not reported")
	}
	if !strings.Contains(got, "entrypoint") || !strings.Contains(got, "--agent-only") {
		t.Fatalf("the message does not point at the entrypoint or show the form: %q", got)
	}

	// Windows resolves the binary name case-insensitively, so the same mistake
	// spelled `Hydra.exe` must still earn the hint.
	got = leftoverArgsMessage([]string{`C:\hydra\Hydra.exe`, "--front-only"})
	if !strings.Contains(got, "entrypoint") {
		t.Fatalf("the hint was withheld from a Windows-cased binary name: %q", got)
	}

	// Any other stray positional is refused too, without the Docker hint.
	got = leftoverArgsMessage([]string{"/etc/hydra/default.toml"})
	if got == "" {
		t.Fatal("a stray positional path was not reported")
	}
	if strings.Contains(got, "entrypoint") {
		t.Fatalf("the Docker hint leaked into an unrelated case: %q", got)
	}
	// Nothing followed it, so nothing was dropped: claiming otherwise sends the
	// reader hunting for a flag that was never passed.
	if strings.Contains(got, "ignored") {
		t.Fatalf("a lone positional was reported as having swallowed flags: %q", got)
	}
}
