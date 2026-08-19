package main

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/agentwire"
)

func TestResolveAgentToken(t *testing.T) {
	cases := []struct {
		name       string
		flagVal    string
		flagSet    bool
		env        string
		cfgVal     string
		wantToken  string
		wantSource string
	}{
		{"nothing set anywhere: no auth", "", false, "", "", "", ""},
		{"config only", "", false, "", "from-config", "from-config", "[daemon] agent_token"},
		{"env only", "", false, "from-env", "", "from-env", "$HYDRA_AGENT_TOKEN"},
		{"flag only", "from-flag", true, "", "", "from-flag", "--agent-token"},
		{"env beats config", "", false, "from-env", "from-config", "from-env", "$HYDRA_AGENT_TOKEN"},
		{"flag beats both", "from-flag", true, "from-env", "from-config", "from-flag", "--agent-token"},
		{"an unset flag never beats the env", "", false, "from-env", "", "from-env", "$HYDRA_AGENT_TOKEN"},
		{"an empty env falls through to the config", "", false, "", "from-config", "from-config", "[daemon] agent_token"},
		{"a blank env falls through to the config", "", false, "   ", "from-config", "from-config", "[daemon] agent_token"},
		{`--agent-token="" turns auth off`, "", true, "from-env", "from-config", "", "--agent-token"},
		{"a secret's trailing newline is not part of the token", "", false, "from-env\n", "", "from-env", "$HYDRA_AGENT_TOKEN"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Setenv(agentwire.TokenEnv, tc.env)
			token, source := resolveAgentToken(tc.flagVal, tc.flagSet, tc.cfgVal)
			if token != tc.wantToken {
				t.Errorf("token = %q, want %q", token, tc.wantToken)
			}
			if source != tc.wantSource {
				t.Errorf("source = %q, want %q", source, tc.wantSource)
			}
		})
	}
}
