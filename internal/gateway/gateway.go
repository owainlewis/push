// Package gateway wires the poller, store, Claude runner, and sender into the
// main poll loop with per-thread serialization.
package gateway

import (
	"context"
	"log"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/owainlewis/push/internal/claude"
	"github.com/owainlewis/push/internal/config"
	"github.com/owainlewis/push/internal/imessage"
	"github.com/owainlewis/push/internal/memory"
	"github.com/owainlewis/push/internal/store"
)

// Gateway runs the message loop.
type Gateway struct {
	cfg    *config.Config
	store  *store.Store
	poller *imessage.Poller
	sender *imessage.Sender
	runner *claude.Runner

	self  map[string]bool // self_handles set
	allow map[string]bool // allow_from set

	mu     sync.Mutex
	queues map[string]chan job // per-thread work queues
	wg     sync.WaitGroup
}

type job struct {
	thread string
	target string // handle to reply to
	text   string
}

// New constructs a Gateway from its dependencies.
func New(cfg *config.Config, st *store.Store, p *imessage.Poller, s *imessage.Sender, r *claude.Runner) *Gateway {
	g := &Gateway{
		cfg:    cfg,
		store:  st,
		poller: p,
		sender: s,
		runner: r,
		self:   toSet(cfg.SelfHandles),
		allow:  toSet(cfg.AllowFrom),
		queues: map[string]chan job{},
	}
	return g
}

// Run polls until ctx is cancelled.
func (g *Gateway) Run(ctx context.Context) error {
	interval, _ := g.cfg.PollDuration()

	// Skip backlog: on first run, start from the current max ROWID.
	if g.store.LastRow() == 0 {
		if max, err := g.poller.MaxRowID(ctx); err == nil {
			_ = g.store.SetLastRow(max)
			log.Printf("starting from ROWID %d (skipping backlog)", max)
		}
	}

	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	log.Printf("push gateway running, polling every %s", interval)

	for {
		select {
		case <-ctx.Done():
			g.drain()
			return ctx.Err()
		case <-ticker.C:
			g.tick(ctx)
		}
	}
}

func (g *Gateway) tick(ctx context.Context) {
	msgs, err := g.poller.Poll(ctx, g.store.LastRow())
	if err != nil {
		log.Printf("poll error: %v", err)
		return
	}
	var maxRow int64
	for _, m := range msgs {
		if m.RowID > maxRow {
			maxRow = m.RowID
		}
		thread, target, ok := g.accept(m)
		if !ok {
			continue
		}
		g.enqueue(ctx, job{thread: thread, target: target, text: strings.TrimSpace(m.Text)})
	}
	if maxRow > 0 {
		if err := g.store.SetLastRow(maxRow); err != nil {
			log.Printf("save state error: %v", err)
		}
	}
}

// accept decides whether a message should be processed and returns the thread
// key and reply target.
func (g *Gateway) accept(m imessage.Message) (thread, target string, ok bool) {
	text := strings.TrimSpace(m.Text)
	if text == "" {
		return "", "", false
	}
	// Never react to our own replies.
	if g.cfg.ReplyMarker != "" && strings.Contains(m.Text, g.cfg.ReplyMarker) {
		return "", "", false
	}
	// Self-chat: you texting yourself.
	if g.self[m.ChatIdentifier] {
		return "self:" + m.ChatIdentifier, m.ChatIdentifier, true
	}
	// Inbound DM from an allowed sender.
	if !m.IsFromMe && g.allow[m.Handle] {
		return "dm:" + m.Handle, m.Handle, true
	}
	return "", "", false
}

// enqueue routes a job to its thread worker, starting the worker on demand.
func (g *Gateway) enqueue(ctx context.Context, j job) {
	g.mu.Lock()
	q, ok := g.queues[j.thread]
	if !ok {
		q = make(chan job, 32)
		g.queues[j.thread] = q
		g.wg.Add(1)
		go g.worker(ctx, j.thread, q)
	}
	g.mu.Unlock()

	select {
	case q <- j:
	default:
		log.Printf("[%s] queue full, dropping message", j.thread)
	}
}

// worker processes one thread's jobs strictly in order.
func (g *Gateway) worker(ctx context.Context, thread string, q chan job) {
	defer g.wg.Done()
	for {
		select {
		case <-ctx.Done():
			return
		case j := <-q:
			g.handle(ctx, j)
		}
	}
}

func (g *Gateway) handle(ctx context.Context, j job) {
	if reply, handled := g.command(j); handled {
		g.reply(ctx, j.target, reply)
		return
	}

	sessionID, isNew, err := g.store.SessionFor(j.thread)
	if err != nil {
		log.Printf("[%s] session error: %v", j.thread, err)
		return
	}

	workDir := g.sandbox(j.thread)
	if err := ensureDir(workDir); err != nil {
		log.Printf("[%s] sandbox error: %v", j.thread, err)
		return
	}

	reply, err := g.runner.Run(ctx, claude.Request{
		SessionID:    sessionID,
		IsNew:        isNew,
		WorkDir:      workDir,
		SystemAppend: memory.Load(g.cfg.AssistantDir),
		Prompt:       j.text,
	})
	if err != nil {
		log.Printf("[%s] claude error: %v", j.thread, err)
		g.reply(ctx, j.target, "⚠️ "+claudeErrorMessage(err))
		return
	}
	_ = g.store.MarkStarted(j.thread)
	g.reply(ctx, j.target, reply)
}

// command handles gateway-level slash commands before anything reaches Claude.
func (g *Gateway) command(j job) (reply string, handled bool) {
	switch strings.ToLower(strings.TrimSpace(j.text)) {
	case "/clear", "/new", "/reset":
		if err := g.store.Rotate(j.thread); err != nil {
			return "Couldn't reset the conversation.", true
		}
		return "Started a fresh conversation.", true
	case "/help":
		return "Commands:\n/clear — start a fresh conversation\n/help — this message", true
	default:
		return "", false
	}
}

func (g *Gateway) reply(ctx context.Context, target, text string) {
	if strings.TrimSpace(text) == "" {
		return
	}
	out := text + g.cfg.ReplyMarker
	if err := g.sender.Send(ctx, target, out); err != nil {
		log.Printf("send error to %s: %v", target, err)
	}
}

// sandbox returns the per-thread working directory.
func (g *Gateway) sandbox(thread string) string {
	return filepath.Join(g.cfg.SessionsDir, sanitize(thread))
}

func (g *Gateway) drain() {
	g.mu.Lock()
	for _, q := range g.queues {
		close(q)
	}
	g.mu.Unlock()
}

func ensureDir(path string) error {
	return os.MkdirAll(path, 0o755)
}

// claudeErrorMessage extracts a short, user-facing reason from a runner error.
func claudeErrorMessage(err error) string {
	msg := err.Error()
	if i := strings.LastIndex(msg, ": "); i >= 0 && i+2 < len(msg) {
		msg = msg[i+2:]
	}
	if strings.TrimSpace(msg) == "" {
		return "couldn't reach Claude"
	}
	return msg
}

func toSet(items []string) map[string]bool {
	m := make(map[string]bool, len(items))
	for _, s := range items {
		if s = strings.TrimSpace(s); s != "" {
			m[s] = true
		}
	}
	return m
}

// sanitize turns a thread key into a filesystem-safe directory name.
func sanitize(s string) string {
	var b strings.Builder
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9', r == '-', r == '_', r == '.':
			b.WriteRune(r)
		default:
			b.WriteRune('_')
		}
	}
	return b.String()
}
