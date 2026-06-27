// Package imessage reads incoming messages from the macOS Messages database and
// sends replies through the Messages app.
package imessage

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
)

// Message is one inbound iMessage row relevant to the gateway.
type Message struct {
	RowID          int64
	GUID           string
	Handle         string // sender handle (phone/email); empty for self-sent
	ChatIdentifier string // the chat's identifier (handle for 1:1 chats)
	Text           string
	IsFromMe       bool
}

// Poller reads new messages from chat.db using the sqlite3 CLI.
type Poller struct {
	dbPath string
}

// NewPoller returns a Poller bound to a chat.db path.
func NewPoller(dbPath string) *Poller {
	return &Poller{dbPath: dbPath}
}

// row mirrors the JSON shape returned by `sqlite3 -json`.
type row struct {
	RowID          int64  `json:"rowid"`
	GUID           string `json:"guid"`
	Text           string `json:"text"`
	TextNull       int    `json:"text_null"`
	IsFromMe       int    `json:"is_from_me"`
	Handle         string `json:"handle"`
	ChatIdentifier string `json:"chat_identifier"`
}

const selectNew = `SELECT m.ROWID AS rowid, m.guid AS guid,
  CASE WHEN m.text IS NULL THEN '' ELSE m.text END AS text,
  CASE WHEN m.text IS NULL THEN 1 ELSE 0 END AS text_null,
  m.is_from_me AS is_from_me,
  COALESCE(h.id,'') AS handle,
  COALESCE(c.chat_identifier,'') AS chat_identifier
FROM message m
LEFT JOIN handle h ON m.handle_id = h.ROWID
LEFT JOIN chat_message_join cmj ON cmj.message_id = m.ROWID
LEFT JOIN chat c ON c.ROWID = cmj.chat_id
WHERE m.ROWID > %d
ORDER BY m.ROWID ASC;`

// Poll returns messages with ROWID greater than sinceRowID, oldest first. When
// a row's text column is NULL, it decodes the attributedBody blob.
func (p *Poller) Poll(ctx context.Context, sinceRowID int64) ([]Message, error) {
	out, err := p.query(ctx, fmt.Sprintf(selectNew, sinceRowID))
	if err != nil {
		return nil, err
	}
	if strings.TrimSpace(string(out)) == "" {
		return nil, nil
	}
	var rows []row
	if err := json.Unmarshal(out, &rows); err != nil {
		return nil, fmt.Errorf("parse sqlite json: %w", err)
	}
	msgs := make([]Message, 0, len(rows))
	for _, r := range rows {
		text := r.Text
		if r.TextNull == 1 {
			if t, err := p.attributedText(ctx, r.RowID); err == nil {
				text = t
			}
		}
		msgs = append(msgs, Message{
			RowID:          r.RowID,
			GUID:           r.GUID,
			Handle:         r.Handle,
			ChatIdentifier: r.ChatIdentifier,
			Text:           text,
			IsFromMe:       r.IsFromMe == 1,
		})
	}
	return msgs, nil
}

// MaxRowID returns the highest message ROWID currently in the database, or 0
// when the table is empty. Used to skip backlog on first run.
func (p *Poller) MaxRowID(ctx context.Context) (int64, error) {
	out, err := p.query(ctx, "SELECT COALESCE(MAX(ROWID),0) AS rowid FROM message;")
	if err != nil {
		return 0, err
	}
	var rows []struct {
		RowID int64 `json:"rowid"`
	}
	if err := json.Unmarshal(out, &rows); err != nil || len(rows) == 0 {
		return 0, err
	}
	return rows[0].RowID, nil
}

// attributedText fetches and decodes the attributedBody blob for one row.
func (p *Poller) attributedText(ctx context.Context, rowID int64) (string, error) {
	out, err := p.query(ctx, fmt.Sprintf("SELECT COALESCE(hex(attributedBody),'') AS hexbody FROM message WHERE ROWID=%d;", rowID))
	if err != nil {
		return "", err
	}
	var rows []struct {
		HexBody string `json:"hexbody"`
	}
	if err := json.Unmarshal(out, &rows); err != nil || len(rows) == 0 || rows[0].HexBody == "" {
		return "", fmt.Errorf("no attributedBody for row %d", rowID)
	}
	raw, err := hex.DecodeString(rows[0].HexBody)
	if err != nil {
		return "", err
	}
	return decodeAttributedBody(raw), nil
}

func (p *Poller) query(ctx context.Context, sql string) ([]byte, error) {
	cmd := exec.CommandContext(ctx, "sqlite3", "-readonly", "-json", p.dbPath, sql)
	out, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("sqlite3 query: %w", err)
	}
	return out, nil
}
