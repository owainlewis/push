package imessage

import "testing"

// makeBlob builds a minimal typedstream-like blob: the "NSString" marker, a
// 0x2b run marker, a length prefix, then the UTF-8 text.
func makeBlob(prefixLen byte, lenBytes []byte, text string) []byte {
	b := []byte("\x04\x0bstreamtyped\x81NSString\x01\x94\x84\x01")
	b = append(b, 0x2b)
	b = append(b, lenBytes...)
	b = append(b, text...)
	_ = prefixLen
	return b
}

func TestDecodeAttributedBody_ShortString(t *testing.T) {
	want := "hello world 👋"
	blob := makeBlob(0, []byte{byte(len(want))}, want)
	if got := decodeAttributedBody(blob); got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestDecodeAttributedBody_TwoByteLength(t *testing.T) {
	want := ""
	for i := 0; i < 300; i++ {
		want += "x"
	}
	n := len(want)
	blob := makeBlob(0, []byte{0x81, byte(n & 0xff), byte(n >> 8)}, want)
	if got := decodeAttributedBody(blob); got != want {
		t.Fatalf("got len %d, want len %d", len(got), len(want))
	}
}

func TestDecodeAttributedBody_NoMarker(t *testing.T) {
	if got := decodeAttributedBody([]byte("nothing useful here")); got != "" {
		t.Fatalf("expected empty, got %q", got)
	}
}

func TestDecodeAttributedBody_Empty(t *testing.T) {
	if got := decodeAttributedBody(nil); got != "" {
		t.Fatalf("expected empty, got %q", got)
	}
}
