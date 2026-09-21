package aurix

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"
)

// SSEEvent is one message from `GET /v1/events`.
type SSEEvent struct {
	// Type is the `event:` field — an event type, "stream.open" or "lagged".
	Type string
	// ID is the `id:` field when present.
	ID string
	// Data is the raw `data:` payload; for regular events it is an EventEnvelope (see Envelope).
	Data []byte
}

// Envelope decodes Data as an EventEnvelope.
func (e SSEEvent) Envelope() (*EventEnvelope, error) {
	var env EventEnvelope
	if err := json.Unmarshal(e.Data, &env); err != nil {
		return nil, err
	}
	return &env, nil
}

// EventStreamOptions configures Client.Events.
type EventStreamOptions struct {
	// Types filters event types (server default: everything except high-frequency events).
	Types []string
	// Reconnect re-establishes the stream after it ends or fails (default true; set NoReconnect).
	NoReconnect bool
	// ReconnectDelay is the initial delay (default 1 s, doubling up to MaxReconnectDelay).
	ReconnectDelay    time.Duration
	MaxReconnectDelay time.Duration
	// LastEventID resumes from a known position on the first connection.
	LastEventID string
	// Request options (headers, credentials).
	RequestOptions []RequestOption
}

// Events streams the application's events, calling fn for each one until ctx is cancelled, fn
// returns an error, or a non-retryable HTTP error occurs. On a "lagged" event fetch
// `/v1/events/snapshot` to resynchronise.
func (c *Client) Events(ctx context.Context, opts EventStreamOptions, fn func(SSEEvent) error) error {
	q := url.Values{}
	if len(opts.Types) > 0 {
		q.Set("types", strings.Join(opts.Types, ","))
	}
	u := c.URL("/v1/events", q)
	delay := opts.ReconnectDelay
	if delay <= 0 {
		delay = time.Second
	}
	maxDelay := opts.MaxReconnectDelay
	if maxDelay <= 0 {
		maxDelay = 30 * time.Second
	}
	initial := delay
	lastID := opts.LastEventID
	var rc requestConfig
	for _, o := range opts.RequestOptions {
		o(&rc)
	}
	// The stream is long-lived: never apply the JSON client's overall timeout to it.
	httpClient := &http.Client{Transport: c.http.Transport, CheckRedirect: c.http.CheckRedirect, Jar: c.http.Jar}
	for {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
		if err != nil {
			return err
		}
		req.Header.Set("Accept", "text/event-stream")
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
		for k, vs := range rc.headers {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}
		if lastID != "" {
			req.Header.Set("Last-Event-ID", lastID)
		}
		resp, err := httpClient.Do(req)
		if err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			if opts.NoReconnect {
				return &NetworkError{Method: "GET", Path: "/v1/events", Err: err}
			}
			if err := sleepCtx(ctx, delay); err != nil {
				return err
			}
			delay = min(maxDelay, delay*2)
			continue
		}
		if resp.StatusCode < 200 || resp.StatusCode >= 300 {
			body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<16))
			_ = resp.Body.Close()
			ct := resp.Header.Get("Content-Type")
			apiErr := errorFromResponse(resp.StatusCode, ct, body, resp.Header, "GET", "/v1/events")
			if !opts.NoReconnect && (resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode >= 500) {
				d := delay
				if apiErr.RetryAfter > 0 {
					d = apiErr.RetryAfter
				}
				if err := sleepCtx(ctx, d); err != nil {
					return err
				}
				delay = min(maxDelay, delay*2)
				continue
			}
			return apiErr
		}
		delay = initial
		err = readSSE(resp.Body, func(ev sseRaw) error {
			if ev.id != "" {
				lastID = ev.id
			}
			if ev.retry > 0 {
				delay = time.Duration(ev.retry) * time.Millisecond
			}
			typ := ev.typ
			if typ == "" {
				typ = "message"
			}
			return fn(SSEEvent{Type: typ, ID: ev.id, Data: []byte(ev.data)})
		})
		_ = resp.Body.Close()
		if err != nil && ctx.Err() == nil && !isStreamEnd(err) {
			return err
		}
		if ctx.Err() != nil {
			return ctx.Err()
		}
		if opts.NoReconnect {
			return nil
		}
		if err := sleepCtx(ctx, delay); err != nil {
			return err
		}
		delay = min(maxDelay, delay*2)
	}
}

type sseRaw struct {
	typ   string
	id    string
	data  string
	retry int
}

type streamEndError struct{ err error }

func (e streamEndError) Error() string { return e.err.Error() }

func isStreamEnd(err error) bool {
	_, ok := err.(streamEndError)
	return ok
}

// readSSE parses text/event-stream from r (comments, multi-line data, id, event, retry) and
// calls emit per event. Transport errors mid-stream are wrapped as streamEndError so the
// caller can reconnect; errors from emit are returned as-is.
func readSSE(r io.Reader, emit func(sseRaw) error) error {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	var cur sseRaw
	var data []string
	dispatch := func() error {
		if len(data) == 0 && cur.typ == "" && cur.id == "" {
			cur, data = sseRaw{}, nil
			return nil
		}
		cur.data = strings.Join(data, "\n")
		err := emit(cur)
		cur, data = sseRaw{}, nil
		return err
	}
	for sc.Scan() {
		line := strings.TrimRight(sc.Text(), "\r")
		if line == "" {
			if err := dispatch(); err != nil {
				return err
			}
			continue
		}
		if strings.HasPrefix(line, ":") {
			continue
		}
		field, value, _ := strings.Cut(line, ":")
		value = strings.TrimPrefix(value, " ")
		switch field {
		case "event":
			cur.typ = value
		case "data":
			data = append(data, value)
		case "id":
			if !strings.ContainsRune(value, 0) {
				cur.id = value
			}
		case "retry":
			if n, err := strconv.Atoi(value); err == nil && n >= 0 {
				cur.retry = n
			}
		}
	}
	if err := sc.Err(); err != nil {
		return streamEndError{err}
	}
	return nil
}
