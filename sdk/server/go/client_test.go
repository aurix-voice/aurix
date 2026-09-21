package aurix

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
	"time"
)

type call struct {
	Method string
	Path   string
	Query  map[string][]string
	Header http.Header
	Body   map[string]any
}

type fakeNode struct {
	srv   *httptest.Server
	mu    sync.Mutex
	calls []call
}

func newFake(t *testing.T, handler func(w http.ResponseWriter, r *http.Request, n int)) *fakeNode {
	f := &fakeNode{}
	f.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		c := call{Method: r.Method, Path: r.URL.Path, Query: r.URL.Query(), Header: r.Header.Clone()}
		if len(raw) > 0 {
			_ = json.Unmarshal(raw, &c.Body)
		}
		f.mu.Lock()
		f.calls = append(f.calls, c)
		n := len(f.calls)
		f.mu.Unlock()
		handler(w, r, n)
	}))
	t.Cleanup(f.srv.Close)
	return f
}

func (f *fakeNode) client(t *testing.T, creds Credentials) *Client {
	c, err := New(Options{BaseURL: f.srv.URL + "/", Credentials: creds, MaxBackoff: 20 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func respond(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}

var tokenBody = map[string]any{
	"token": "jwt", "user_id": "11111111-0000-4000-8000-000000000001", "expires_at": "2030-01-01T00:00:00Z", "channels": []any{},
	"endpoint": map[string]any{"region": "eu_west", "node_id": "22222222-0000-4000-8000-000000000002", "ws_url": "wss://node/ws", "probe_url": nil, "location": nil, "distance_km": nil},
}

func TestIssueTokenSendsAPIKeyAndBody(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) { respond(w, 200, tokenBody) })
	c := f.client(t, Credentials{APIKey: "ak_test"})
	region := RegionEuWest
	tok, err := c.IssueToken(context.Background(), GenerateTokenRequest{ExternalID: "player-1", DisplayName: "Player", Region: &region})
	if err != nil {
		t.Fatal(err)
	}
	if tok.Token != "jwt" || tok.Endpoint == nil || tok.Endpoint.WSURL != "wss://node/ws" {
		t.Fatalf("unexpected response %+v", tok)
	}
	c0 := f.calls[0]
	if c0.Method != "POST" || c0.Path != "/v1/tokens" || c0.Header.Get("X-API-Key") != "ak_test" || c0.Header.Get("Authorization") != "" {
		t.Fatalf("bad request %+v", c0)
	}
	if c0.Body["external_id"] != "player-1" || c0.Body["display_name"] != "Player" || c0.Body["region"] != "eu_west" {
		t.Fatalf("bad body %+v", c0.Body)
	}
	if _, present := c0.Body["channels"]; present {
		t.Fatalf("omitted optional field must not be sent: %+v", c0.Body)
	}
	if !strings.HasPrefix(c0.Header.Get("User-Agent"), "aurix-server-sdk-go/") {
		t.Fatalf("user agent %q", c0.Header.Get("User-Agent"))
	}
}

func TestPathAndQueryEncoding(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		respond(w, 200, map[string]any{"messages": []any{}, "next_before": nil, "next_after": nil})
	})
	c := f.client(t, Credentials{APIKey: "k"})
	limit := int64(10)
	before := "a/b c"
	if _, err := c.ListChannelMessages(context.Background(), "ch/1 x", &ListChannelMessagesQuery{Limit: &limit, Before: &before}); err != nil {
		t.Fatal(err)
	}
	c0 := f.calls[0]
	if c0.Path != "/v1/channels/ch%2F1%20x/messages" && c0.Path != "/v1/channels/ch/1 x/messages" {
		t.Fatalf("path %q", c0.Path)
	}
	if c0.Query["limit"][0] != "10" || c0.Query["before"][0] != "a/b c" {
		t.Fatalf("query %+v", c0.Query)
	}
}

func TestBearerAuthAndPerCallOverride(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		respond(w, 200, map[string]any{"status": "ok", "version": "1", "uptime_seconds": 1, "active_sessions": 0, "active_channels": 0})
	})
	c := f.client(t, Credentials{AdminToken: "adm"})
	if _, err := c.Health(context.Background()); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Health(context.Background(), WithCredentials(Credentials{PlayerToken: "ply"}), WithHeader("X-Trace", "1")); err != nil {
		t.Fatal(err)
	}
	if f.calls[0].Header.Get("Authorization") != "Bearer adm" || f.calls[1].Header.Get("Authorization") != "Bearer ply" || f.calls[1].Header.Get("X-Trace") != "1" {
		t.Fatalf("headers %+v %+v", f.calls[0].Header, f.calls[1].Header)
	}
}

func TestErrorEnvelope(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		w.Header().Set("X-Request-Id", "req-1")
		respond(w, 404, map[string]any{"error": map[string]any{"code": "NOT_FOUND", "message": "channel missing"}})
	})
	c := f.client(t, Credentials{APIKey: "k"})
	_, err := c.GetChannel(context.Background(), "x")
	e, ok := AsError(err)
	if !ok || e.Status != 404 || e.Code != "NOT_FOUND" || e.Message != "channel missing" || e.RequestID != "req-1" || !e.IsNotFound() {
		t.Fatalf("error %v", err)
	}
}

func TestRetriesIdempotentAndRateLimited(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, n int) {
		switch n {
		case 1:
			w.WriteHeader(503)
		case 2:
			w.Header().Set("Retry-After", "0")
			w.WriteHeader(429)
		default:
			respond(w, 200, map[string]any{"channels": []any{}, "total": 0})
		}
	})
	c := f.client(t, Credentials{APIKey: "k"})
	if _, err := c.ListChannels(context.Background(), nil); err != nil {
		t.Fatal(err)
	}
	if len(f.calls) != 3 {
		t.Fatalf("calls %d", len(f.calls))
	}
}

func TestNoRetryForPostOn503(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) { w.WriteHeader(503) })
	c := f.client(t, Credentials{APIKey: "k"})
	_, err := c.IssueToken(context.Background(), GenerateTokenRequest{ExternalID: "u", DisplayName: "U"})
	if e, ok := AsError(err); !ok || e.Status != 503 || len(f.calls) != 1 {
		t.Fatalf("err %v calls %d", err, len(f.calls))
	}
}

func TestTimeoutIsNetworkError(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		select {
		case <-r.Context().Done():
		case <-time.After(2 * time.Second):
		}
	})
	c, _ := New(Options{BaseURL: f.srv.URL, Credentials: Credentials{APIKey: "k"}, HTTPClient: &http.Client{Timeout: 50 * time.Millisecond}, MaxRetries: new(int)})
	_, err := c.Health(context.Background())
	var ne *NetworkError
	if !errors.As(err, &ne) {
		t.Fatalf("expected NetworkError, got %v", err)
	}
}

func TestRawAndTypedContentType(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		w.Header().Set("Content-Type", "text/csv; charset=utf-8")
		w.WriteHeader(200)
		_, _ = io.WriteString(w, "a,b\n1,2\n")
	})
	c := f.client(t, Credentials{APIKey: "k"})
	raw, err := c.ExportUsageRaw(context.Background(), nil)
	if err != nil || raw.ContentType != "text/csv" || string(raw.Body) != "a,b\n1,2\n" {
		t.Fatalf("raw %+v %v", raw, err)
	}
	if f.calls[0].Header.Get("Accept") != "*/*" {
		t.Fatalf("accept %q", f.calls[0].Header.Get("Accept"))
	}
	_, err = c.ExportUsage(context.Background(), nil)
	if e, ok := AsError(err); !ok || e.Code != "unexpected_content_type" {
		t.Fatalf("typed call on csv must fail: %v", err)
	}
}

func TestNoContent(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, _ int) { w.WriteHeader(204) })
	c := f.client(t, Credentials{APIKey: "k"})
	if err := c.DeleteWebhook(context.Background(), "c1"); err != nil {
		t.Fatal(err)
	}
	if f.calls[0].Method != "DELETE" || f.calls[0].Path != "/v1/webhooks/c1" {
		t.Fatalf("%+v", f.calls[0])
	}
}

func TestWebhookSignatureVector(t *testing.T) {
	raw, err := os.ReadFile("../vectors/webhook_signature.json")
	if err != nil {
		t.Fatal(err)
	}
	var v struct {
		Secret       string `json:"secret"`
		Timestamp    int64  `json:"timestamp"`
		Body         string `json:"body"`
		Header       string `json:"header"`
		ToleranceSec int64  `json:"tolerance_sec"`
	}
	if err := json.Unmarshal(raw, &v); err != nil {
		t.Fatal(err)
	}
	if got := SignWebhook(v.Secret, v.Timestamp, []byte(v.Body)); got != v.Header {
		t.Fatalf("sign: %s", got)
	}
	at := func(off int64) *VerifyOptions {
		return &VerifyOptions{Tolerance: time.Duration(v.ToleranceSec) * time.Second, Now: func() time.Time { return time.Unix(v.Timestamp+off, 0) }}
	}
	if !VerifyWebhookSignature(v.Secret, v.Header, []byte(v.Body), at(10)) {
		t.Fatal("valid signature rejected")
	}
	if VerifyWebhookSignature(v.Secret, v.Header, []byte(v.Body+" "), at(10)) {
		t.Fatal("tampered body accepted")
	}
	if VerifyWebhookSignature("other", v.Header, []byte(v.Body), at(10)) {
		t.Fatal("wrong secret accepted")
	}
	if VerifyWebhookSignature(v.Secret, v.Header, []byte(v.Body), at(v.ToleranceSec+1)) {
		t.Fatal("stale signature accepted")
	}
	if VerifyWebhookSignature(v.Secret, "v1=abc", []byte(v.Body), at(0)) || VerifyWebhookSignature(v.Secret, "", []byte(v.Body), at(0)) {
		t.Fatal("malformed header accepted")
	}
	h := http.Header{}
	h.Set(SignatureHeader, v.Header)
	h.Set(EventHeader, "participant.joined")
	h.Set(DeliveryIDHeader, "d1")
	h.Set(AttemptHeader, "2")
	d, err := ParseWebhook(v.Secret, h, []byte(v.Body), at(0))
	if err != nil || d.Event.Type != "participant.joined" || d.DeliveryID != "d1" || d.Attempt != 2 {
		t.Fatalf("parse %+v %v", d, err)
	}
	h.Set(EventHeader, "participant.left")
	if _, err := ParseWebhook(v.Secret, h, []byte(v.Body), at(0)); err == nil {
		t.Fatal("event header mismatch accepted")
	}
}

func TestSSEParser(t *testing.T) {
	raw := ": keepalive\n\nevent: stream.open\ndata: {\"ok\":true}\n\nid: 42\r\nevent: participant.joined\r\ndata: {\"id\":\"e1\",\r\ndata: \"type\":\"x\"}\r\n\r\nretry: 250\ndata: tail\n\n"
	var got []sseRaw
	if err := readSSE(strings.NewReader(raw), func(e sseRaw) error { got = append(got, e); return nil }); err != nil {
		t.Fatal(err)
	}
	if len(got) != 3 || got[0].typ != "stream.open" || got[0].data != `{"ok":true}` {
		t.Fatalf("%+v", got)
	}
	if got[1].id != "42" || got[1].typ != "participant.joined" || got[1].data != "{\"id\":\"e1\",\n\"type\":\"x\"}" {
		t.Fatalf("%+v", got[1])
	}
	if got[2].retry != 250 || got[2].data != "tail" || got[2].typ != "" {
		t.Fatalf("%+v", got[2])
	}
}

func TestEventStreamReconnectsWithLastEventID(t *testing.T) {
	f := newFake(t, func(w http.ResponseWriter, r *http.Request, n int) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(200)
		if n == 1 {
			_, _ = io.WriteString(w, "event: stream.open\ndata: {}\n\nid: ev-1\nevent: participant.joined\ndata: {\"id\":\"ev-1\",\"type\":\"participant.joined\"}\n\n")
			return
		}
		_, _ = io.WriteString(w, "id: ev-2\nevent: participant.left\ndata: {\"id\":\"ev-2\",\"type\":\"participant.left\"}\n\n")
		w.(http.Flusher).Flush()
		<-r.Context().Done()
	})
	c := f.client(t, Credentials{APIKey: "k"})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	var seen []string
	err := c.Events(ctx, EventStreamOptions{Types: []string{"participant.joined", "participant.left"}, ReconnectDelay: 5 * time.Millisecond}, func(ev SSEEvent) error {
		seen = append(seen, ev.Type)
		if len(seen) == 3 {
			cancel()
		}
		return nil
	})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err %v", err)
	}
	if strings.Join(seen, ",") != "stream.open,participant.joined,participant.left" {
		t.Fatalf("seen %v", seen)
	}
	if f.calls[0].Query["types"][0] != "participant.joined,participant.left" || f.calls[0].Header.Get("Accept") != "text/event-stream" {
		t.Fatalf("%+v", f.calls[0])
	}
	if f.calls[1].Header.Get("Last-Event-ID") != "ev-1" {
		t.Fatalf("Last-Event-ID %q", f.calls[1].Header.Get("Last-Event-ID"))
	}
}
