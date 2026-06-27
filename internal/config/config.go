// Package config loads push gateway configuration from a JSON file.
package config

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// Config is the on-disk gateway configuration.
type Config struct {
	DBPath         string   `json:"db_path"`
	PollInterval   string   `json:"poll_interval"`
	RunTimeout     string   `json:"run_timeout"`
	SelfHandles    []string `json:"self_handles"`
	AllowFrom      []string `json:"allow_from"`
	ClaudeBin      string   `json:"claude_bin"`
	PermissionMode string   `json:"permission_mode"`
	SessionsDir    string   `json:"sessions_dir"`
	StatePath      string   `json:"state_path"`
	AssistantDir   string   `json:"assistant_dir"`
	ReplyMarker    string   `json:"reply_marker"`
}

// Load reads and validates the config at path, applying defaults and expanding
// a leading "~" in path fields to the user's home directory.
func Load(path string) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	var c Config
	if err := json.Unmarshal(b, &c); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	c.applyDefaults()
	c.DBPath = expandHome(c.DBPath)
	c.SessionsDir = expandHome(c.SessionsDir)
	c.StatePath = expandHome(c.StatePath)
	c.AssistantDir = expandHome(c.AssistantDir)
	if err := c.validate(); err != nil {
		return nil, err
	}
	return &c, nil
}

func (c *Config) applyDefaults() {
	if c.DBPath == "" {
		c.DBPath = "~/Library/Messages/chat.db"
	}
	if c.PollInterval == "" {
		c.PollInterval = "3s"
	}
	if c.RunTimeout == "" {
		c.RunTimeout = "120s"
	}
	if c.ClaudeBin == "" {
		c.ClaudeBin = "claude"
	}
	if c.PermissionMode == "" {
		c.PermissionMode = "bypassPermissions"
	}
	if c.SessionsDir == "" {
		c.SessionsDir = "~/.push/sessions"
	}
	if c.StatePath == "" {
		c.StatePath = "~/.push/state.json"
	}
	if c.AssistantDir == "" {
		c.AssistantDir = "./assistant"
	}
	if c.ReplyMarker == "" {
		c.ReplyMarker = "\n\n— sent by push"
	}
}

func (c *Config) validate() error {
	if len(c.SelfHandles) == 0 && len(c.AllowFrom) == 0 {
		return fmt.Errorf("config: set at least one of self_handles or allow_from, or nobody can reach the assistant")
	}
	if _, err := c.PollDuration(); err != nil {
		return fmt.Errorf("config: invalid poll_interval %q: %w", c.PollInterval, err)
	}
	if _, err := c.RunDuration(); err != nil {
		return fmt.Errorf("config: invalid run_timeout %q: %w", c.RunTimeout, err)
	}
	return nil
}

// PollDuration parses PollInterval into a time.Duration.
func (c *Config) PollDuration() (time.Duration, error) {
	return time.ParseDuration(c.PollInterval)
}

// RunDuration parses RunTimeout into a time.Duration.
func (c *Config) RunDuration() (time.Duration, error) {
	return time.ParseDuration(c.RunTimeout)
}

func expandHome(p string) string {
	if p == "~" || strings.HasPrefix(p, "~/") {
		home, err := os.UserHomeDir()
		if err == nil {
			return filepath.Join(home, strings.TrimPrefix(p, "~"))
		}
	}
	return p
}
