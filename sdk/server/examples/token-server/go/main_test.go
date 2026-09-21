package main

import (
	"bytes"
	"encoding/json"
	"log"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	aurix "github.com/aurix-voice/aurix/sdk/server/go"
)

const (
	apiKey = "aurx_test_SECRET_KEY_never_in_client_payload"
	secret = "0123456789abcdef0123456789abcdef"
)

type seen struct {
	Path   string
	APIKey string
	Body   map[string]any
}

type fakeAurix struct {
	*httptest.Server
	mu   sync.Mutex
	seen []seen
}

func newFakeAurix(t *testing.T) *fakeAurix {
	f := &fakeAurix{}
	f.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		_ = json.NewDecoder(r.Body).Decode(&body)
		f.mu.Lock()
		f.seen = append(f.seen, seen{Path: r.URL.Path, APIKey: r.Header.Get("X-API-Key"), Body: body})
		f.mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		switch {
		case r.URL.Path != "/v1/tokens":
			w.WriteHeader(404)
			_, _ = w.Write([]byte(`{"error":{"code":"NOT_FOUND","message":"nope"}}`))
		case r.Header.Get("X-API-Key") != apiKey:
			w.WriteHeader(401)
			_, _ = w.Write([]byte(`{"error":{"code":"AUTH_FAILED","message":"bad key"}}`))
		default:
			_, _ = w.Write([]byte(`{"token":"player.jwt","user_id":"u-1","expires_at":"2030-01-01T00:00:00Z","channels":[],
				"endpoint":{"region":"eu_west","node_id":"n1","ws_url":"wss://eu1.example/ws","nodes":1,"load_factor":0.1},
				"api_key_echo":"` + apiKey + `"}`))
		}
	}))
	t.Cleanup(f.Close)
	return f
}

func (f *fakeAurix) last(t *testing.T) seen {
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.seen) == 0 {
		t.Fatal("aurix was not called")
	}
	return f.seen[len(f.seen)-1]
}

func (f *fakeAurix) calls() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.seen)
}

func env(m map[string]string) func(string) string { return func(k string) string { return m[k] } }

func newStack(t *testing.T, key string, logs *bytes.Buffer) (*fakeAurix, *httptest.Server) {
	fa := newFakeAurix(t)
	cfg, err := LoadConfig(env(map[string]string{
		"AURIX_URL": fa.URL, "AURIX_API_KEY": key, "GAME_SESSION_SECRET": secret,
		"AURIX_REGION": "eu_west", "ALLOW_DEV_LOGIN": "1",
	}))
	if err != nil {
		t.Fatal(err)
	}
	var logger *log.Logger
	if logs != nil {
		logger = log.New(logs, "", 0)
	}
	srv, err := NewServer(cfg, nil, logger)
	if err != nil {
		t.Fatal(err)
	}
	ts := httptest.NewServer(srv.Handler())
	t.Cleanup(ts.Close)
	return fa, ts
}

func post(t *testing.T, url string, body any, session string) (int, map[string]any) {
	raw, _ := json.Marshal(body)
	req, _ := http.NewRequest(http.MethodPost, url, bytes.NewReader(raw))
	req.Header.Set("Content-Type", "application/json")
	if session != "" {
		req.Header.Set("Authorization", "Bearer "+session)
	}
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	var out map[string]any
	_ = json.NewDecoder(res.Body).Decode(&out)
	return res.StatusCode, out
}

func TestConfigRefusesUnsafeStartup(t *testing.T) {
	for name, m := range map[string]map[string]string{
		"no key":       {"GAME_SESSION_SECRET": secret},
		"weak secret":  {"AURIX_API_KEY": apiKey, "GAME_SESSION_SECRET": "short"},
		"bad region":   {"AURIX_API_KEY": apiKey, "GAME_SESSION_SECRET": secret, "AURIX_REGION": "eu"},
		"bad port":     {"AURIX_API_KEY": apiKey, "GAME_SESSION_SECRET": secret, "PORT": "x"},
		"missing file": {"AURIX_API_KEY_FILE": "/nonexistent/key", "GAME_SESSION_SECRET": secret},
	} {
		if _, err := LoadConfig(env(m)); err == nil {
			t.Errorf("%s: expected error", name)
		}
	}
}

func TestGameSessionRejectsForgedAndExpired(t *testing.T) {
	good := MintDevSession(secret, "p1", "Alice", time.Hour)
	if p, ok := AuthenticatePlayer(secret, "Bearer "+good); !ok || p.ID != "p1" || p.DisplayName != "Alice" {
		t.Fatalf("valid session rejected: %+v %v", p, ok)
	}
	for name, hdr := range map[string]string{
		"other secret": "Bearer " + MintDevSession(strings.Repeat("x", 32), "p1", "Alice", time.Hour),
		"bad sig":      "Bearer " + strings.Split(good, ".")[0] + ".AAAA",
		"expired":      "Bearer " + MintDevSession(secret, "p1", "Alice", -time.Second),
		"missing":      "",
	} {
		if _, ok := AuthenticatePlayer(secret, hdr); ok {
			t.Errorf("%s: accepted", name)
		}
	}
}

func TestTokenRequiresSession(t *testing.T) {
	fa, ts := newStack(t, apiKey, nil)
	status, _ := post(t, ts.URL+"/voice/token", map[string]any{"match_id": "m1", "external_id": "admin"}, "")
	if status != 401 || fa.calls() != 0 {
		t.Fatalf("status %d, aurix calls %d", status, fa.calls())
	}
}

func TestHappyPathHidesAPIKey(t *testing.T) {
	fa, ts := newStack(t, apiKey, nil)
	status, login := post(t, ts.URL+"/dev/login", map[string]string{"player_id": "p1", "display_name": "Alice"}, "")
	if status != 200 {
		t.Fatalf("login %d", status)
	}
	status, body := post(t, ts.URL+"/voice/token", map[string]any{"match_id": "m1", "external_id": "spoof", "channels": []string{"*"}}, login["session"].(string))
	if status != 200 {
		t.Fatalf("token %d %v", status, body)
	}
	raw, _ := json.Marshal(body)
	want := `{"endpoint":{"region":"eu_west","ws_url":"wss://eu1.example/ws"},"expires_at":"2030-01-01T00:00:00Z","token":"player.jwt","user_id":"u-1"}`
	if string(raw) != want {
		t.Fatalf("client payload %s", raw)
	}
	if strings.Contains(string(raw), apiKey) {
		t.Fatal("api key leaked to client")
	}
	s := fa.last(t)
	if s.Path != "/v1/tokens" || s.APIKey != apiKey {
		t.Fatalf("upstream %+v", s)
	}
	if s.Body["external_id"] != "p1" || s.Body["display_name"] != "Alice" || s.Body["region"] != "eu_west" {
		t.Fatalf("upstream body %v", s.Body)
	}
	var grants []aurix.ChannelGrant
	ch, _ := json.Marshal(s.Body["channels"])
	if err := json.Unmarshal(ch, &grants); err != nil || len(grants) != 1 {
		t.Fatalf("grants %s: %v", ch, err)
	}
	g := grants[0]
	if g.AdHoc == nil || g.AdHoc.Name != "match-m1" || g.AdHoc.ChannelType != aurix.ChannelTypeTeam ||
		g.ChannelID != nil || g.Moderate != nil || g.Priority != nil ||
		!*g.Join || !*g.Speak || !*g.Receive {
		t.Fatalf("grants %s", ch)
	}
}

func TestInvalidMatchRefusedBeforeAurix(t *testing.T) {
	fa, ts := newStack(t, apiKey, nil)
	status, _ := post(t, ts.URL+"/voice/token", map[string]any{"match_id": "../etc"}, MintDevSession(secret, "p1", "Alice", time.Hour))
	if status != 403 || fa.calls() != 0 {
		t.Fatalf("status %d, aurix calls %d", status, fa.calls())
	}
}

func TestAurixErrorIsGenericAndLogged(t *testing.T) {
	var logs bytes.Buffer
	_, ts := newStack(t, "aurx_wrong_key", &logs)
	status, body := post(t, ts.URL+"/voice/token", map[string]any{"match_id": "m1"}, MintDevSession(secret, "p1", "A", time.Hour))
	if status != 502 || body["error"] != "voice service unavailable" {
		t.Fatalf("%d %v", status, body)
	}
	if !strings.Contains(logs.String(), "aurix 401 AUTH_FAILED") || strings.Contains(logs.String(), "aurx_wrong_key") {
		t.Fatalf("logs: %q", logs.String())
	}
}

var _ TokenIssuer = (*aurix.Client)(nil)
