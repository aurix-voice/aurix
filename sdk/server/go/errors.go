package aurix

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"
)

// Error is a non-2xx response. Code and Message come from the API's
// `{"error": {"code", "message"}}` envelope when present.
type Error struct {
	Status     int
	Code       string
	Message    string
	Method     string
	Path       string
	RequestID  string
	RetryAfter time.Duration
	// Body is the raw response body.
	Body []byte
}

func (e *Error) Error() string {
	return fmt.Sprintf("aurix: %s %s -> %d %s: %s", e.Method, e.Path, e.Status, e.Code, e.Message)
}

// IsAuth reports 401/403.
func (e *Error) IsAuth() bool {
	return e.Status == http.StatusUnauthorized || e.Status == http.StatusForbidden
}

// IsNotFound reports 404.
func (e *Error) IsNotFound() bool { return e.Status == http.StatusNotFound }

// IsRateLimited reports 429.
func (e *Error) IsRateLimited() bool { return e.Status == http.StatusTooManyRequests }

// NetworkError is a transport failure (DNS, refused connection, timeout, cancelled context) —
// no HTTP response was received.
type NetworkError struct {
	Method string
	Path   string
	Err    error
}

func (e *NetworkError) Error() string {
	return fmt.Sprintf("aurix: %s %s: %v", e.Method, e.Path, e.Err)
}

func (e *NetworkError) Unwrap() error { return e.Err }

// AsError returns the *Error inside err, if any.
func AsError(err error) (*Error, bool) {
	var e *Error
	ok := errors.As(err, &e)
	return e, ok
}

func errorFromResponse(status int, contentType string, body []byte, h http.Header, method, path string) *Error {
	e := &Error{
		Status:    status,
		Code:      fmt.Sprintf("http_%d", status),
		Message:   fmt.Sprintf("HTTP %d", status),
		Method:    method,
		Path:      path,
		RequestID: h.Get("X-Request-Id"),
		Body:      body,
	}
	if len(body) > 0 {
		msg := string(body)
		if len(msg) > 512 {
			msg = msg[:512]
		}
		e.Message = msg
	}
	if strings.HasPrefix(strings.ToLower(contentType), "application/json") {
		var env struct {
			Error struct {
				Code    string `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		}
		if json.Unmarshal(body, &env) == nil {
			if env.Error.Code != "" {
				e.Code = env.Error.Code
			}
			if env.Error.Message != "" {
				e.Message = env.Error.Message
			}
		}
	}
	if ra, ok := RetryAfter(h); ok {
		e.RetryAfter = ra
	}
	return e
}
