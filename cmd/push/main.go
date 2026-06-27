// Command push is a tiny iMessage gateway for Claude Code. It polls the macOS
// Messages database for new messages, runs each through `claude -p` with
// persistent per-conversation sessions and injected memory, and texts the reply
// back.
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/owainlewis/push/internal/claude"
	"github.com/owainlewis/push/internal/config"
	"github.com/owainlewis/push/internal/gateway"
	"github.com/owainlewis/push/internal/imessage"
	"github.com/owainlewis/push/internal/store"
)

// shutdownGrace bounds how long we wait for in-flight runs on exit.
const shutdownGrace = 30 * time.Second

func main() {
	configPath := flag.String("config", "config.json", "path to config file")
	flag.Parse()

	cfg, err := config.Load(*configPath)
	if err != nil {
		log.Fatalf("config: %v", err)
	}

	if err := preflight(cfg); err != nil {
		log.Fatalf("preflight: %v", err)
	}

	st, err := store.Open(cfg.StatePath)
	if err != nil {
		log.Fatalf("state: %v", err)
	}

	g := gateway.New(
		cfg,
		st,
		imessage.NewPoller(cfg.DBPath),
		imessage.NewSender(),
		claude.New(cfg.ClaudeBin, cfg.PermissionMode),
	)

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	if err := g.Run(ctx); err != nil {
		log.Fatalf("gateway: %v", err)
	}

	log.Println("draining in-flight runs...")
	g.Shutdown(shutdownGrace)
	log.Println("shut down cleanly")
}

// preflight fails fast with actionable messages when the environment is not
// ready, rather than surfacing opaque errors at the first message.
func preflight(cfg *config.Config) error {
	// Messages database must be readable (requires Full Disk Access).
	f, err := os.Open(cfg.DBPath)
	if err != nil {
		if os.IsPermission(err) {
			return fmt.Errorf("cannot read %s: grant Full Disk Access to your terminal in System Settings -> Privacy & Security -> Full Disk Access", cfg.DBPath)
		}
		if os.IsNotExist(err) {
			return fmt.Errorf("Messages database not found at %s; is iMessage set up on this Mac?", cfg.DBPath)
		}
		return fmt.Errorf("open %s: %w", cfg.DBPath, err)
	}
	_ = f.Close()

	// Required external tools.
	for _, bin := range []string{cfg.ClaudeBin, "sqlite3", "osascript"} {
		if _, err := exec.LookPath(bin); err != nil {
			return fmt.Errorf("%q not found on PATH: %w", bin, err)
		}
	}

	// Writable working directories.
	if err := os.MkdirAll(cfg.SessionsDir, 0o755); err != nil {
		return fmt.Errorf("sessions_dir %s not writable: %w", cfg.SessionsDir, err)
	}
	if err := os.MkdirAll(filepath.Dir(cfg.StatePath), 0o755); err != nil {
		return fmt.Errorf("state_path dir not writable: %w", err)
	}

	return nil
}
