package hydra

import "embed"

// WebAssets bundles the UI (templates + static) and the changelog into the
// binary so Hydra is self-contained — no files need to sit next to it on disk
// (bare-metal installs, agent nodes, etc.). Consumed by internal/api.
//
//go:embed web/static web/templates CHANGELOG.md
var WebAssets embed.FS
