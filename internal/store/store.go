// Package store persists gateway state: the last processed message ROWID and
// the mapping from conversation thread to Claude Code session UUID.
package store

import (
	"crypto/rand"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
)

// SessionInfo is the Claude Code session bound to a thread.
type SessionInfo struct {
	UUID string `json:"uuid"`
	// Started is true once at least one claude run has used this UUID, so
	// subsequent runs use --resume instead of --session-id.
	Started bool `json:"started"`
}

// Store is the persisted gateway state. It is safe for concurrent use.
type Store struct {
	mu        sync.Mutex
	path      string
	LastRowID int64                  `json:"last_row_id"`
	Sessions  map[string]SessionInfo `json:"sessions"`
}

// Open loads the store at path, creating an empty one if the file is missing.
func Open(path string) (*Store, error) {
	s := &Store{path: path, Sessions: map[string]SessionInfo{}}
	b, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return s, nil
		}
		return nil, fmt.Errorf("read state: %w", err)
	}
	if err := json.Unmarshal(b, s); err != nil {
		return nil, fmt.Errorf("parse state: %w", err)
	}
	if s.Sessions == nil {
		s.Sessions = map[string]SessionInfo{}
	}
	return s, nil
}

// LastRow returns the last processed message ROWID.
func (s *Store) LastRow() int64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.LastRowID
}

// SetLastRow records the last processed message ROWID and persists state.
func (s *Store) SetLastRow(id int64) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if id <= s.LastRowID {
		return nil
	}
	s.LastRowID = id
	return s.saveLocked()
}

// SessionFor returns the session UUID for a thread, creating one if needed.
// isNew is true when the UUID has not yet been started (caller should use
// --session-id rather than --resume).
func (s *Store) SessionFor(thread string) (uuid string, isNew bool, err error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	si, ok := s.Sessions[thread]
	if !ok {
		si = SessionInfo{UUID: newUUID()}
		s.Sessions[thread] = si
		if err := s.saveLocked(); err != nil {
			return "", false, err
		}
	}
	return si.UUID, !si.Started, nil
}

// MarkStarted records that a thread's session has been used at least once.
func (s *Store) MarkStarted(thread string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	si, ok := s.Sessions[thread]
	if !ok || si.Started {
		return nil
	}
	si.Started = true
	s.Sessions[thread] = si
	return s.saveLocked()
}

// Rotate assigns a fresh session UUID to a thread (the /clear behavior).
func (s *Store) Rotate(thread string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.Sessions[thread] = SessionInfo{UUID: newUUID()}
	return s.saveLocked()
}

func (s *Store) saveLocked() error {
	if err := os.MkdirAll(filepath.Dir(s.path), 0o755); err != nil {
		return fmt.Errorf("create state dir: %w", err)
	}
	b, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	tmp := s.path + ".tmp"
	if err := os.WriteFile(tmp, b, 0o600); err != nil {
		return fmt.Errorf("write state: %w", err)
	}
	return os.Rename(tmp, s.path)
}

// newUUID returns a random RFC 4122 version 4 UUID string.
func newUUID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(fmt.Sprintf("uuid: %v", err))
	}
	b[6] = (b[6] & 0x0f) | 0x40 // version 4
	b[8] = (b[8] & 0x3f) | 0x80 // variant 10
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}
