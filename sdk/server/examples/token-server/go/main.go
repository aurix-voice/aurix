// Backend-only Aurix token server.
//
//	game client --(game session)--> POST /voice/token --> this server --(API key)--> Aurix POST /v1/tokens
//	game client <-- { token, user_id, expires_at, endpoint } <---------------------------'
//
// The API key lives only in this process' environment. Clients never see it, never choose their own
// player id (that comes from the game session) and never choose their grants.
package main

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"regexp"
	"strconv"
	"strings"
	"time"

	aurix "github.com/aurix-voice/aurix/sdk/server/go"
)

const maxBody = 4096

var (
	regions = map[aurix.Region]bool{
		aurix.RegionUsEast: true, aurix.RegionUsWest: true, aurix.RegionEuWest: true,
		aurix.RegionEuCentral: true, aurix.RegionAsiaPacific: true, aurix.RegionSouthAmerica: true,
		aurix.RegionAustralia: true, aurix.RegionMiddleEast: true, aurix.RegionAfrica: true,
	}
	matchID = regexp.MustCompile(`^[A-Za-z0-9_-]{1,64}$`)
	session = regexp.MustCompile(`^Bearer\s+([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)$`)
)

// Config is read from the environment; see LoadConfig.
type Config struct {
	AurixURL      string
	APIKey        string
	SessionSecret string
	Region        *aurix.Region
	Port          int
	AllowDevLogin bool
}

// LoadConfig reads configuration from getenv; it fails on anything unsafe or missing.
func LoadConfig(getenv func(string) string) (Config, error) {
	apiKey := strings.TrimSpace(getenv("AURIX_API_KEY"))
	if f := getenv("AURIX_API_KEY_FILE"); f != "" {
		b, err := os.ReadFile(f)
		if err != nil {
			return Config{}, fmt.Errorf("AURIX_API_KEY_FILE: %w", err)
		}
		apiKey = strings.TrimSpace(string(b))
	}
	if apiKey == "" {
		return Config{}, errors.New("set AURIX_API_KEY or AURIX_API_KEY_FILE (backend environment only)")
	}
	secret := getenv("GAME_SESSION_SECRET")
	if len(secret) < 32 {
		return Config{}, errors.New("GAME_SESSION_SECRET must be >= 32 characters")
	}
	cfg := Config{
		AurixURL:      getenv("AURIX_URL"),
		APIKey:        apiKey,
		SessionSecret: secret,
		Port:          3000,
		AllowDevLogin: getenv("ALLOW_DEV_LOGIN") == "1",
	}
	if cfg.AurixURL == "" {
		cfg.AurixURL = "http://localhost:8080"
	}
	if r := getenv("AURIX_REGION"); r != "" {
		if !regions[aurix.Region(r)] {
			return Config{}, fmt.Errorf("AURIX_REGION %q is not a known region", r)
		}
		reg := aurix.Region(r)
		cfg.Region = &reg
	}
	if p := getenv("PORT"); p != "" {
		n, err := strconv.Atoi(p)
		if err != nil {
			return Config{}, fmt.Errorf("PORT: %w", err)
		}
		cfg.Port = n
	}
	return cfg, nil
}

// ---------------------------------------------------------------------------------------------
// Game session — stand-in for your real login. Replace AuthenticatePlayer with your own
// session/JWT validation; what matters is that the player id comes from *your* auth, not the body.
// ---------------------------------------------------------------------------------------------

// Player is the identity established by the game's own authentication.
type Player struct {
	ID          string
	DisplayName string
}

type sessionClaims struct {
	PID  string `json:"pid"`
	Name string `json:"name"`
	Exp  int64  `json:"exp"`
}

var b64 = base64.RawURLEncoding

func sign(secret, payload string) string {
	m := hmac.New(sha256.New, []byte(secret))
	m.Write([]byte(payload))
	return b64.EncodeToString(m.Sum(nil))
}

// MintDevSession issues an HMAC-signed development session (only used by /dev/login).
func MintDevSession(secret, playerID, displayName string, ttl time.Duration) string {
	raw, _ := json.Marshal(sessionClaims{PID: playerID, Name: displayName, Exp: time.Now().Add(ttl).Unix()})
	payload := b64.EncodeToString(raw)
	return payload + "." + sign(secret, payload)
}

// AuthenticatePlayer validates the game session in an Authorization header.
func AuthenticatePlayer(secret, authorization string) (Player, bool) {
	m := session.FindStringSubmatch(authorization)
	if m == nil {
		return Player{}, false
	}
	if !hmac.Equal([]byte(m[2]), []byte(sign(secret, m[1]))) {
		return Player{}, false
	}
	raw, err := b64.DecodeString(m[1])
	if err != nil {
		return Player{}, false
	}
	var c sessionClaims
	if json.Unmarshal(raw, &c) != nil || c.PID == "" || c.Name == "" || c.Exp <= time.Now().Unix() {
		return Player{}, false
	}
	return Player{ID: c.PID, DisplayName: c.Name}, true
}

// ---------------------------------------------------------------------------------------------
// Token issuance
// ---------------------------------------------------------------------------------------------

// TokenIssuer is the part of aurix.Client the server needs (an interface so tests can stub it).
type TokenIssuer interface {
	IssueToken(ctx context.Context, body aurix.GenerateTokenRequest, opts ...aurix.RequestOption) (*aurix.TokenResponse, error)
}

// VoiceToken is the allow-listed response handed to the game client. New Aurix fields must be
// opted in here — nothing from upstream is forwarded implicitly.
type VoiceToken struct {
	Token     string         `json:"token"`
	UserID    string         `json:"user_id"`
	ExpiresAt string         `json:"expires_at"`
	Endpoint  *VoiceEndpoint `json:"endpoint"`
}

// VoiceEndpoint is the node the client should connect to.
type VoiceEndpoint struct {
	WSURL  string       `json:"ws_url"`
	Region aurix.Region `json:"region"`
}

// playerMayJoin is the game-side authorisation hook (stub: any well-formed match).
func playerMayJoin(_ Player, match string) bool { return matchID.MatchString(match) }

// IssueVoiceToken calls Aurix with the backend API key for an authenticated player.
func IssueVoiceToken(ctx context.Context, issuer TokenIssuer, p Player, match string, region *aurix.Region) (*VoiceToken, error) {
	yes := true
	res, err := issuer.IssueToken(ctx, aurix.GenerateTokenRequest{
		ExternalID:  p.ID,
		DisplayName: p.DisplayName,
		Channels: []aurix.ChannelGrant{{
			AdHoc:   &aurix.AdHocChannel{Name: "match-" + match, ChannelType: aurix.ChannelTypeTeam},
			Join:    &yes,
			Speak:   &yes,
			Receive: &yes,
		}},
		Region: region,
	})
	if err != nil {
		return nil, err
	}
	out := &VoiceToken{Token: res.Token, UserID: res.UserID, ExpiresAt: res.ExpiresAt}
	if res.Endpoint != nil {
		out.Endpoint = &VoiceEndpoint{WSURL: res.Endpoint.WSURL, Region: res.Endpoint.Region}
	}
	return out, nil
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

// Server is the token server's HTTP handler.
type Server struct {
	cfg    Config
	issuer TokenIssuer
	logger *log.Logger
}

// NewServer wires the handler; issuer defaults to an aurix.Client for cfg.
func NewServer(cfg Config, issuer TokenIssuer, logger *log.Logger) (*Server, error) {
	if issuer == nil {
		c, err := aurix.New(aurix.Options{BaseURL: cfg.AurixURL, Credentials: aurix.Credentials{APIKey: cfg.APIKey}})
		if err != nil {
			return nil, err
		}
		issuer = c
	}
	if logger == nil {
		logger = log.Default()
	}
	return &Server{cfg: cfg, issuer: issuer, logger: logger}, nil
}

// Handler returns the routed http.Handler.
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet {
			writeJSON(w, http.StatusMethodNotAllowed, map[string]string{"error": "method not allowed"})
			return
		}
		writeJSON(w, http.StatusOK, map[string]bool{"ok": true})
	})
	if s.cfg.AllowDevLogin {
		mux.HandleFunc("/dev/login", s.devLogin)
	}
	mux.HandleFunc("/voice/token", s.voiceToken)
	return mux
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func readJSON(w http.ResponseWriter, r *http.Request, v any) bool {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]string{"error": "method not allowed"})
		return false
	}
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxBody)).Decode(v); err != nil {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid JSON"})
		return false
	}
	return true
}

func (s *Server) devLogin(w http.ResponseWriter, r *http.Request) {
	var body struct {
		PlayerID    string `json:"player_id"`
		DisplayName string `json:"display_name"`
	}
	if !readJSON(w, r, &body) {
		return
	}
	if body.PlayerID == "" || body.DisplayName == "" {
		writeJSON(w, http.StatusBadRequest, map[string]string{"error": "player_id and display_name required"})
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"session": MintDevSession(s.cfg.SessionSecret, body.PlayerID, body.DisplayName, time.Hour)})
}

func (s *Server) voiceToken(w http.ResponseWriter, r *http.Request) {
	player, ok := AuthenticatePlayer(s.cfg.SessionSecret, r.Header.Get("Authorization"))
	if !ok {
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "not logged in"})
		return
	}
	var body struct {
		MatchID string `json:"match_id"`
	}
	if !readJSON(w, r, &body) {
		return
	}
	if !playerMayJoin(player, body.MatchID) {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "not allowed to join this match"})
		return
	}
	tok, err := IssueVoiceToken(r.Context(), s.issuer, player, body.MatchID, s.cfg.Region)
	if err != nil {
		// Aurix' message may describe our request; log it server-side, never forward it verbatim.
		var ae *aurix.Error
		if errors.As(err, &ae) {
			s.logger.Printf("aurix %d %s (request %s)", ae.Status, ae.Code, ae.RequestID)
			status := http.StatusBadGateway
			if ae.IsRateLimited() {
				status = http.StatusServiceUnavailable
			}
			writeJSON(w, status, map[string]string{"error": "voice service unavailable"})
			return
		}
		s.logger.Printf("aurix unreachable: %v", err)
		writeJSON(w, http.StatusServiceUnavailable, map[string]string{"error": "voice service unavailable"})
		return
	}
	writeJSON(w, http.StatusOK, tok)
}

func main() {
	cfg, err := LoadConfig(os.Getenv)
	if err != nil {
		log.Fatal(err)
	}
	srv, err := NewServer(cfg, nil, nil)
	if err != nil {
		log.Fatal(err)
	}
	dev := ""
	if cfg.AllowDevLogin {
		dev = " (dev login enabled)"
	}
	log.Printf("token server on :%d -> %s%s", cfg.Port, cfg.AurixURL, dev)
	h := &http.Server{
		Addr:              ":" + strconv.Itoa(cfg.Port),
		Handler:           srv.Handler(),
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       10 * time.Second,
		WriteTimeout:      15 * time.Second,
	}
	log.Fatal(h.ListenAndServe())
}
