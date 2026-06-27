// Command push is a tiny iMessage gateway for Claude Code. It polls the macOS
// Messages database for new messages, runs each through `claude -p` with
// persistent per-conversation sessions and injected memory, and texts the reply
// back.
package main

import (
	"context"
	"flag"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/owainlewis/push/internal/claude"
	"github.com/owainlewis/push/internal/config"
	"github.com/owainlewis/push/internal/gateway"
	"github.com/owainlewis/push/internal/imessage"
	"github.com/owainlewis/push/internal/store"
)

func main() {
	configPath := flag.String("config", "config.json", "path to config file")
	flag.Parse()

	cfg, err := config.Load(*configPath)
	if err != nil {
		log.Fatalf("config: %v", err)
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

	if err := g.Run(ctx); err != nil && ctx.Err() == nil {
		log.Fatalf("gateway: %v", err)
	}
	log.Println("shutting down")
	os.Exit(0)
}
