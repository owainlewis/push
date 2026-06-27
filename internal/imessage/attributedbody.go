package imessage

import "bytes"

// decodeAttributedBody best-effort extracts the message text from the
// NSAttributedString typedstream blob stored in message.attributedBody.
//
// Recent macOS often leaves message.text NULL and stores the text inside this
// archived blob. The full format is Apple's typedstream, but the displayed
// string is reliably stored as a length-prefixed UTF-8 run just after the
// "NSString" class marker. We parse that run and ignore the rest. This is a
// heuristic: if the layout does not match, it returns "".
func decodeAttributedBody(b []byte) string {
	i := bytes.Index(b, []byte("NSString"))
	if i < 0 {
		return ""
	}
	rest := b[i+len("NSString"):]

	// The string run is introduced by a 0x2b ('+') marker, followed by a
	// length and then the UTF-8 bytes.
	j := bytes.IndexByte(rest, 0x2b)
	if j < 0 || j+1 >= len(rest) {
		return ""
	}
	p := rest[j+1:]

	n, off := readLength(p)
	if n <= 0 || off+n > len(p) {
		return ""
	}
	return string(p[off : off+n])
}

// readLength decodes the typedstream length prefix. Values under 0x80 are a
// single byte; 0x81 introduces a 2-byte little-endian length; 0x82 a 4-byte one.
func readLength(p []byte) (n, off int) {
	if len(p) == 0 {
		return 0, 0
	}
	switch p[0] {
	case 0x81:
		if len(p) < 3 {
			return 0, 0
		}
		return int(p[1]) | int(p[2])<<8, 3
	case 0x82:
		if len(p) < 5 {
			return 0, 0
		}
		return int(p[1]) | int(p[2])<<8 | int(p[3])<<16 | int(p[4])<<24, 5
	default:
		return int(p[0]), 1
	}
}
