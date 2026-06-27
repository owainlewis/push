// Package claude runs the Claude Code CLI headlessly for a single message.
package claude

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
)

// Runner invokes the claude binary in print mode.
type Runner struct {
	Bin            string
	PermissionMode string
}

// New returns a Runner.
func New(bin, permissionMode string) *Runner {
	return &Runner{Bin: bin, PermissionMode: permissionMode}
}

// Request is one headless turn.
type Request struct {
	SessionID    string // session UUID for this conversation
	IsNew        bool   // true => --session-id, false => --resume
	WorkDir      string // sandbox working directory
	SystemAppend string // memory injected via --append-system-prompt
	Prompt       string // the user's message text
}

// result mirrors the JSON object printed by `claude -p --output-format json`.
type result struct {
	Result    string `json:"result"`
	SessionID string `json:"session_id"`
	IsError   bool   `json:"is_error"`
	Subtype   string `json:"subtype"`
}

// Run executes one turn and returns Claude's reply text.
func (r *Runner) Run(ctx context.Context, req Request) (string, error) {
	args := []string{
		"-p", req.Prompt,
		"--output-format", "json",
		"--permission-mode", r.PermissionMode,
		"--add-dir", req.WorkDir,
	}
	if req.IsNew {
		args = append(args, "--session-id", req.SessionID)
	} else {
		args = append(args, "--resume", req.SessionID)
	}
	if strings.TrimSpace(req.SystemAppend) != "" {
		args = append(args, "--append-system-prompt", req.SystemAppend)
	}

	cmd := exec.CommandContext(ctx, r.Bin, args...)
	cmd.Dir = req.WorkDir
	out, err := cmd.Output()
	if err != nil {
		if ee, ok := err.(*exec.ExitError); ok {
			return "", fmt.Errorf("claude exited: %w: %s", err, strings.TrimSpace(string(ee.Stderr)))
		}
		return "", fmt.Errorf("run claude: %w", err)
	}

	var res result
	if err := json.Unmarshal(out, &res); err != nil {
		// Fall back to raw output if it is not the expected JSON envelope.
		return strings.TrimSpace(string(out)), nil
	}
	if res.IsError {
		return "", fmt.Errorf("claude error (%s): %s", res.Subtype, res.Result)
	}
	return strings.TrimSpace(res.Result), nil
}
