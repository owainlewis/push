package imessage

import (
	"context"
	"fmt"
	"os/exec"
)

// Sender sends iMessages through the Messages app via osascript.
type Sender struct{}

// NewSender returns a Sender.
func NewSender() *Sender { return &Sender{} }

// Send delivers text to the given iMessage handle (phone number or email).
//
// The message and target are passed as AppleScript argv rather than
// interpolated into the script, so no escaping of quotes, backslashes, or
// newlines is needed.
func (s *Sender) Send(ctx context.Context, target, text string) error {
	args := []string{
		"-e", "on run argv",
		"-e", "set msg to item 1 of argv",
		"-e", "set target to item 2 of argv",
		"-e", `tell application "Messages"`,
		"-e", "set theService to 1st account whose service type = iMessage",
		"-e", "set theBuddy to participant target of theService",
		"-e", "send msg to theBuddy",
		"-e", "end tell",
		"-e", "end run",
		"--", text, target,
	}
	cmd := exec.CommandContext(ctx, "osascript", args...)
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("osascript send: %w: %s", err, out)
	}
	return nil
}
