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

	self       map[string]bool // self_handles set
	allow      map[string]bool // allow_from set
	runTimeout time.Duration   // max wall-clock per claude run

	mu     sync.Mutex
	queues map[string]chan job // per-thread work queues
	wg     sync.WaitGroup
}

// sendTimeout bounds a single osascript reply.
const sendTimeout = 30 * time.Second

type job struct {
	thread string
	target string // handle to reply to
	text   string
}

// New constructs a Gateway from its dependencies.
func New(cfg *config.Config, st *store.Store, p *imessage.Poller, s *imessage.Sender, r *claude.Runner) *Gateway {
	runTimeout, _ := cfg.RunDuration()
	g := &Gateway{
		cfg:        cfg,
		store:      st,
		poller:     p,
		sender:     s,
		runner:     r,
		self:       toSet(cfg.SelfHandles),
		allow:      toSet(cfg.AllowFrom),
		runTimeout: runTimeout,
		queues:     map[string]chan job{},
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
			// Stop polling. In-flight runs are drained by Shutdown.
			return nil
		case <-ticker.C:
			g.tick(ctx)
		}
	}
}

// Shutdown stops accepting new work and waits up to grace for in-flight runs to
// finish so a reply is never lost mid-send.
func (g *Gateway) Shutdown(grace time.Duration) {
	g.mu.Lock()
	for _, q := range g.queues {
		close(q)
	}
	g.mu.Unlock()

	done := make(chan struct{})
	go func() {
		g.wg.Wait()
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(grace):
		log.Printf("shutdown: grace period expired with runs still in flight")
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
		g.enqueue(job{thread: thread, target: target, text: strings.TrimSpace(m.Text)})
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

// enqueue routes a job to its thread worker, starting the worker on demand. If
// the thread's queue is full it tells the sender instead of dropping silently.
func (g *Gateway) enqueue(j job) {
	g.mu.Lock()
	q, ok := g.queues[j.thread]
	if !ok {
		q = make(chan job, 32)
		g.queues[j.thread] = q
		g.wg.Add(1)
		go g.worker(q)
	}
	g.mu.Unlock()

	select {
	case q <- j:
	default:
		log.Printf("[%s] queue full, asking sender to resend", j.thread)
		g.reply(j.target, "I'm a bit behind on this thread — resend that in a moment.")
	}
}

// worker processes one thread's jobs strictly in order, draining the queue when
// it is closed during shutdown.
func (g *Gateway) worker(q chan job) {
	defer g.wg.Done()
	for j := range q {
		g.handle(j)
	}
}

func (g *Gateway) handle(j job) {
	if reply, handled := g.command(j); handled {
		g.reply(j.target, reply)
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

	// Runs use a background-derived timeout so an OS signal stops new polling
	// but does not kill a reply mid-flight; the run is bounded by runTimeout.
	runCtx, cancel := context.WithTimeout(context.Background(), g.runTimeout)
	defer cancel()

	reply, err := g.runner.Run(runCtx, claude.Request{
		SessionID:    sessionID,
		IsNew:        isNew,
		WorkDir:      workDir,
		SystemAppend: memory.Load(g.cfg.AssistantDir),
		Prompt:       j.text,
	})
	if err != nil {
		if runCtx.Err() == context.DeadlineExceeded {
			log.Printf("[%s] claude run timed out after %s", j.thread, g.runTimeout)
			g.reply(j.target, "That took too long and was stopped. Try again or simplify the request.")
			return
		}
		log.Printf("[%s] claude error: %v", j.thread, err)
		g.reply(j.target, "⚠️ "+claudeErrorMessage(err))
		return
	}
	_ = g.store.MarkStarted(j.thread)
	g.reply(j.target, reply)
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

func (g *Gateway) reply(target, text string) {
	if strings.TrimSpace(text) == "" {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), sendTimeout)
	defer cancel()
	out := text + g.cfg.ReplyMarker
	if err := g.sender.Send(ctx, target, out); err != nil {
		log.Printf("send error to %s: %v", target, err)
	}
}

// sandbox returns the per-thread working directory.
func (g *Gateway) sandbox(thread string) string {
	return filepath.Join(g.cfg.SessionsDir, sanitize(thread))
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
