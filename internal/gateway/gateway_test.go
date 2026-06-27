package gateway

import (
	"testing"

	"github.com/owainlewis/push/internal/config"
	"github.com/owainlewis/push/internal/imessage"
)

func testGateway() *Gateway {
	cfg := &config.Config{
		SelfHandles: []string{"me@icloud.com"},
		AllowFrom:   []string{"+15551234567"},
		ReplyMarker: "\n\n— sent by push",
		RunTimeout:  "120s",
	}
	return New(cfg, nil, nil, nil, nil)
}

func TestAccept(t *testing.T) {
	g := testGateway()

	cases := []struct {
		name       string
		msg        imessage.Message
		wantOK     bool
		wantThread string
		wantTarget string
	}{
		{
			name:       "self-chat accepted",
			msg:        imessage.Message{ChatIdentifier: "me@icloud.com", IsFromMe: true, Text: "hi"},
			wantOK:     true,
			wantThread: "self:me@icloud.com",
			wantTarget: "me@icloud.com",
		},
		{
			name:       "allowlisted dm accepted",
			msg:        imessage.Message{ChatIdentifier: "+15551234567", Handle: "+15551234567", IsFromMe: false, Text: "hi"},
			wantOK:     true,
			wantThread: "dm:+15551234567",
			wantTarget: "+15551234567",
		},
		{
			name:   "non-allowlisted dm dropped",
			msg:    imessage.Message{ChatIdentifier: "+19998887777", Handle: "+19998887777", IsFromMe: false, Text: "hi"},
			wantOK: false,
		},
		{
			name:   "our own reply dropped via marker",
			msg:    imessage.Message{ChatIdentifier: "me@icloud.com", IsFromMe: true, Text: "an answer\n\n— sent by push"},
			wantOK: false,
		},
		{
			name:   "is_from_me to someone else dropped",
			msg:    imessage.Message{ChatIdentifier: "+12223334444", Handle: "+12223334444", IsFromMe: true, Text: "hey friend"},
			wantOK: false,
		},
		{
			name:   "empty text dropped",
			msg:    imessage.Message{ChatIdentifier: "me@icloud.com", IsFromMe: true, Text: "   "},
			wantOK: false,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			thread, target, ok := g.accept(tc.msg)
			if ok != tc.wantOK {
				t.Fatalf("ok = %v, want %v", ok, tc.wantOK)
			}
			if !ok {
				return
			}
			if thread != tc.wantThread {
				t.Errorf("thread = %q, want %q", thread, tc.wantThread)
			}
			if target != tc.wantTarget {
				t.Errorf("target = %q, want %q", target, tc.wantTarget)
			}
		})
	}
}
