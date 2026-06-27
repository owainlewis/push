// Package memory loads the user-owned assistant context (User.md, Memory.md)
// that the gateway injects into every Claude Code run.
package memory

import (
	"os"
	"path/filepath"
	"strings"
)

// files are loaded in order and concatenated for injection.
var files = []string{"User.md", "Memory.md"}

// Load reads the assistant memory files from dir and returns them joined into a
// single block suitable for --append-system-prompt. Missing or empty files are
// skipped. A missing directory yields an empty string with no error.
func Load(dir string) string {
	var parts []string
	for _, name := range files {
		b, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil {
			continue
		}
		if s := strings.TrimSpace(string(b)); s != "" {
			parts = append(parts, s)
		}
	}
	return strings.Join(parts, "\n\n")
}
