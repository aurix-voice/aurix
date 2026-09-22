// Package aurix is the server-side (game backend) SDK for the Aurix voice platform REST API:
// token issuance, channels, moderation, analytics, webhooks and the SSE event stream.
//
// Typed operations (client_gen.go / types_gen.go) are generated from api/openapi.json and keep
// the OpenAPI operation ids; this file is the hand-written transport underneath them.
package aurix

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/rand"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// SDKVersion is reported in the User-Agent header.
const SDKVersion = "1.3.0"

// Credentials selects how requests authenticate. Set exactly one field; precedence when several
// are set: APIKey, AdminToken, PlayerToken, BootstrapToken.
type Credentials struct {
	// APIKey is an application API key (`X-API-Key`). Game backends use this — it must never
	// leave the server.
	APIKey string
	// AdminToken is an operator JWT from `POST /admin/login` (Bearer).
	AdminToken string
	// PlayerToken is a player session JWT (Bearer) for acting on behalf of a player.
	PlayerToken string
	// BootstrapToken unlocks `POST /admin/setup` on a fresh deployment (`X-Bootstrap-Token`).
	BootstrapToken string
}

func (c Credentials) apply(h http.Header) {
	switch {
	case c.APIKey != "":
		h.Set("X-API-Key", c.APIKey)
	case c.AdminToken != "":
		h.Set("Authorization", "Bearer "+c.AdminToken)
	case c.PlayerToken != "":
		h.Set("Authorization", "Bearer "+c.PlayerToken)
	case c.BootstrapToken != "":
		h.Set("X-Bootstrap-Token", c.BootstrapToken)
	}
}

// Options configures a Client.
type Options struct {
	// BaseURL is the node's HTTP origin, e.g. "https://voice.example.com".
	BaseURL string
	Credentials
	// HTTPClient defaults to a client with a 15 s timeout.
	HTTPClient *http.Client
	// MaxRetries bounds automatic retries (default 2). Idempotent methods are retried on network
	// errors and 502/503/504; every method is retried on 429 (honouring Retry-After).
	MaxRetries *int
	// MaxBackoff caps the delay between retries (default 5 s).
	MaxBackoff time.Duration
	// Headers are added to every request.
	Headers   http.Header
	UserAgent string
}

// Client talks to one Aurix deployment. Construct it with New.
type Client struct {
	baseURL    string
	creds      Credentials
	http       *http.Client
	maxRetries int
	maxBackoff time.Duration
	headers    http.Header
	userAgent  string
}

// New creates a client; BaseURL is required.
func New(opts Options) (*Client, error) {
	if opts.BaseURL == "" {
		return nil, errors.New("aurix: BaseURL is required")
	}
	if _, err := url.Parse(opts.BaseURL); err != nil {
		return nil, fmt.Errorf("aurix: invalid BaseURL: %w", err)
	}
	c := &Client{
		baseURL:    strings.TrimRight(opts.BaseURL, "/"),
		creds:      opts.Credentials,
		http:       opts.HTTPClient,
		maxRetries: 2,
		maxBackoff: opts.MaxBackoff,
		headers:    opts.Headers.Clone(),
		userAgent:  opts.UserAgent,
	}
	if c.http == nil {
		c.http = &http.Client{Timeout: 15 * time.Second}
	}
	if opts.MaxRetries != nil && *opts.MaxRetries >= 0 {
		c.maxRetries = *opts.MaxRetries
	}
	if c.maxBackoff <= 0 {
		c.maxBackoff = 5 * time.Second
	}
	if c.userAgent == "" {
		c.userAgent = "aurix-server-sdk-go/" + SDKVersion
	}
	return c, nil
}

// BaseURL returns the configured origin without a trailing slash.
func (c *Client) BaseURL() string { return c.baseURL }

// RequestOption customises a single call.
type RequestOption func(*requestConfig)

type requestConfig struct {
	headers http.Header
	creds   *Credentials
}

// WithHeader adds a header to one request.
func WithHeader(key, value string) RequestOption {
	return func(rc *requestConfig) {
		if rc.headers == nil {
			rc.headers = http.Header{}
		}
		rc.headers.Add(key, value)
	}
}

// WithCredentials overrides the client credentials for one request (e.g. an admin token on a
// client that normally uses an API key).
func WithCredentials(creds Credentials) RequestOption {
	return func(rc *requestConfig) { rc.creds = &creds }
}

// RawResponse is an undecoded 2xx response for binary / CSV / SRT / VTT operations.
type RawResponse struct {
	Status      int
	ContentType string
	Header      http.Header
	Body        []byte
}

// URL builds an absolute URL for path and query (used by the SSE helper and for debugging).
func (c *Client) URL(path string, q url.Values) string {
	u := c.baseURL + path
	if len(q) > 0 {
		u += "?" + q.Encode()
	}
	return u
}

var idempotent = map[string]bool{"GET": true, "HEAD": true, "OPTIONS": true, "PUT": true, "DELETE": true}

func (c *Client) doJSON(ctx context.Context, method, path string, q url.Values, body any, out any, opts []RequestOption) error {
	raw, err := c.doRaw(ctx, method, path, q, body, opts, "application/json")
	if err != nil {
		return err
	}
	if out == nil || raw.Status == http.StatusNoContent || len(raw.Body) == 0 {
		return nil
	}
	if !strings.HasPrefix(strings.ToLower(raw.ContentType), "application/json") {
		return &Error{
			Status:  raw.Status,
			Code:    "unexpected_content_type",
			Message: fmt.Sprintf("expected application/json, got %q; use the *Raw variant", raw.ContentType),
			Method:  method,
			Path:    path,
			Body:    raw.Body,
		}
	}
	if err := json.Unmarshal(raw.Body, out); err != nil {
		return &Error{Status: raw.Status, Code: "invalid_json", Message: err.Error(), Method: method, Path: path, Body: raw.Body}
	}
	return nil
}

func (c *Client) doRaw(ctx context.Context, method, path string, q url.Values, body any, opts []RequestOption, accept string) (*RawResponse, error) {
	var rc requestConfig
	for _, o := range opts {
		o(&rc)
	}
	var payload []byte
	if body != nil {
		var err error
		if payload, err = json.Marshal(body); err != nil {
			return nil, fmt.Errorf("aurix: encode %s %s body: %w", method, path, err)
		}
	}
	u := c.URL(path, q)
	for attempt := 0; ; attempt++ {
		var reader io.Reader
		if payload != nil {
			reader = bytes.NewReader(payload)
		}
		req, err := http.NewRequestWithContext(ctx, method, u, reader)
		if err != nil {
			return nil, fmt.Errorf("aurix: build request: %w", err)
		}
		req.Header.Set("Accept", accept)
		req.Header.Set("User-Agent", c.userAgent)
		for k, vs := range c.headers {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}
		if rc.creds != nil {
			rc.creds.apply(req.Header)
		} else {
			c.creds.apply(req.Header)
		}
		if payload != nil {
			req.Header.Set("Content-Type", "application/json")
		}
		for k, vs := range rc.headers {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}

		resp, err := c.http.Do(req)
		if err != nil {
			if ctx.Err() != nil {
				return nil, &NetworkError{Method: method, Path: path, Err: ctx.Err()}
			}
			if attempt < c.maxRetries && idempotent[method] {
				if err := sleepCtx(ctx, c.backoff(attempt)); err != nil {
					return nil, &NetworkError{Method: method, Path: path, Err: err}
				}
				continue
			}
			return nil, &NetworkError{Method: method, Path: path, Err: err}
		}
		data, readErr := io.ReadAll(resp.Body)
		_ = resp.Body.Close()
		if readErr != nil {
			if attempt < c.maxRetries && idempotent[method] {
				if err := sleepCtx(ctx, c.backoff(attempt)); err != nil {
					return nil, &NetworkError{Method: method, Path: path, Err: err}
				}
				continue
			}
			return nil, &NetworkError{Method: method, Path: path, Err: readErr}
		}
		ct := resp.Header.Get("Content-Type")
		if i := strings.IndexByte(ct, ';'); i >= 0 {
			ct = strings.TrimSpace(ct[:i])
		}
		if resp.StatusCode >= 200 && resp.StatusCode < 300 {
			return &RawResponse{Status: resp.StatusCode, ContentType: ct, Header: resp.Header, Body: data}, nil
		}
		retryable := resp.StatusCode == http.StatusTooManyRequests ||
			((resp.StatusCode == 502 || resp.StatusCode == 503 || resp.StatusCode == 504) && idempotent[method])
		if retryable && attempt < c.maxRetries {
			delay := c.backoff(attempt)
			if ra, ok := RetryAfter(resp.Header); ok {
				delay = ra
				if delay > c.maxBackoff {
					delay = c.maxBackoff
				}
			}
			if err := sleepCtx(ctx, delay); err != nil {
				return nil, &NetworkError{Method: method, Path: path, Err: err}
			}
			continue
		}
		return nil, errorFromResponse(resp.StatusCode, ct, data, resp.Header, method, path)
	}
}

// RetryAfter parses a Retry-After header given in seconds.
func RetryAfter(h http.Header) (time.Duration, bool) {
	v := h.Get("Retry-After")
	if v == "" {
		return 0, false
	}
	secs, err := strconv.ParseFloat(v, 64)
	if err != nil || secs < 0 {
		return 0, false
	}
	return time.Duration(secs * float64(time.Second)), true
}

func (c *Client) backoff(attempt int) time.Duration {
	base := 200 * time.Millisecond << uint(attempt)
	if base > c.maxBackoff || base <= 0 {
		base = c.maxBackoff
	}
	return time.Duration(float64(base) * (0.5 + rand.Float64()/2)) //nolint:gosec // jitter only
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}
