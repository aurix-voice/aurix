package aurix

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// Webhook header names.
const (
	SignatureHeader  = "X-Aurix-Signature"
	EventHeader      = "X-Aurix-Event"
	WebhookIDHeader  = "X-Aurix-Webhook-Id"
	DeliveryIDHeader = "X-Aurix-Delivery-Id"
	AttemptHeader    = "X-Aurix-Attempt"
)

// DefaultTolerance is the default replay window for webhook timestamps.
const DefaultTolerance = 5 * time.Minute

// ErrInvalidSignature is returned by ParseWebhook when the signature is missing, malformed,
// stale or does not match.
var ErrInvalidSignature = errors.New("aurix: invalid or expired X-Aurix-Signature")

// SignWebhook computes the `X-Aurix-Signature` value for body at ts (unix seconds):
// `t=<ts>,v1=hex(HMAC-SHA256(secret, "<ts>.<body>"))`. Useful for tests and re-signing proxies.
func SignWebhook(secret string, ts int64, body []byte) string {
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(strconv.FormatInt(ts, 10)))
	mac.Write([]byte("."))
	mac.Write(body)
	return "t=" + strconv.FormatInt(ts, 10) + ",v1=" + hex.EncodeToString(mac.Sum(nil))
}

// VerifyOptions tunes VerifyWebhookSignature.
type VerifyOptions struct {
	// Tolerance is the accepted |now - t| (default DefaultTolerance).
	Tolerance time.Duration
	// Now overrides the clock (tests).
	Now func() time.Time
}

// VerifyWebhookSignature reports whether header is a valid signature of the raw body within the
// replay window. Compare against the exact bytes received, never a re-serialised JSON object.
func VerifyWebhookSignature(secret, header string, body []byte, opts *VerifyOptions) bool {
	if secret == "" || header == "" {
		return false
	}
	var ts int64
	var haveTS bool
	var v1 string
	for _, part := range strings.Split(header, ",") {
		k, v, ok := strings.Cut(strings.TrimSpace(part), "=")
		if !ok {
			continue
		}
		switch k {
		case "t":
			n, err := strconv.ParseInt(v, 10, 64)
			if err == nil {
				ts, haveTS = n, true
			}
		case "v1":
			v1 = strings.ToLower(strings.TrimSpace(v))
		}
	}
	if !haveTS || v1 == "" {
		return false
	}
	tolerance := DefaultTolerance
	now := time.Now
	if opts != nil {
		if opts.Tolerance > 0 {
			tolerance = opts.Tolerance
		}
		if opts.Now != nil {
			now = opts.Now
		}
	}
	diff := now().Unix() - ts
	if diff < 0 {
		diff = -diff
	}
	if diff > int64(tolerance/time.Second) {
		return false
	}
	expected := SignWebhook(secret, ts, body)
	expected = expected[strings.Index(expected, "v1=")+3:]
	got, err := hex.DecodeString(v1)
	if err != nil {
		return false
	}
	want, _ := hex.DecodeString(expected)
	return hmac.Equal(got, want)
}

// IncomingWebhook is one verified webhook POST.
type IncomingWebhook struct {
	Event EventEnvelope
	// WebhookID is `X-Aurix-Webhook-Id`.
	WebhookID string
	// DeliveryID is `X-Aurix-Delivery-Id` — stable across retries; use it or Event.ID for
	// idempotent processing.
	DeliveryID string
	// Attempt is `X-Aurix-Attempt` (1-based).
	Attempt int
}

// ParseWebhook verifies and decodes one webhook request. body must be the exact bytes received.
func ParseWebhook(secret string, h http.Header, body []byte, opts *VerifyOptions) (*IncomingWebhook, error) {
	if !VerifyWebhookSignature(secret, h.Get(SignatureHeader), body, opts) {
		return nil, ErrInvalidSignature
	}
	var env EventEnvelope
	if err := json.Unmarshal(body, &env); err != nil {
		return nil, fmt.Errorf("aurix: decode webhook body: %w", err)
	}
	if t := h.Get(EventHeader); t != "" && t != env.Type {
		return nil, fmt.Errorf("aurix: %s %q does not match body type %q", EventHeader, t, env.Type)
	}
	attempt := 1
	if n, err := strconv.Atoi(h.Get(AttemptHeader)); err == nil && n > 0 {
		attempt = n
	}
	return &IncomingWebhook{Event: env, WebhookID: h.Get(WebhookIDHeader), DeliveryID: h.Get(DeliveryIDHeader), Attempt: attempt}, nil
}
