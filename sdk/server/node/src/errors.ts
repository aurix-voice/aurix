/** Non-2xx response. `code`/`message` come from the API's `{ "error": { code, message } }` envelope. */
export class AurixError extends Error {
  readonly status: number;
  readonly code: string;
  readonly method: string;
  readonly path: string;
  readonly requestId: string | undefined;
  readonly retryAfterMs: number | undefined;
  /** Raw response body (JSON-decoded when possible). */
  readonly body: unknown;

  constructor(init: {
    status: number;
    code: string;
    message: string;
    method: string;
    path: string;
    requestId?: string;
    retryAfterMs?: number;
    body?: unknown;
  }) {
    super(`${init.method} ${init.path} → ${init.status} ${init.code}: ${init.message}`);
    this.name = "AurixError";
    this.status = init.status;
    this.code = init.code;
    this.method = init.method;
    this.path = init.path;
    this.requestId = init.requestId;
    this.retryAfterMs = init.retryAfterMs;
    this.body = init.body;
  }

  /** True for 401/403. */
  get isAuth(): boolean {
    return this.status === 401 || this.status === 403;
  }

  get isNotFound(): boolean {
    return this.status === 404;
  }

  get isRateLimited(): boolean {
    return this.status === 429;
  }

  static fromResponse(status: number, contentType: string, text: string, headers: Headers, method: string, path: string): AurixError {
    let code = `http_${status}`;
    let message = text.slice(0, 512) || `HTTP ${status}`;
    let body: unknown = text;
    if (contentType.toLowerCase().startsWith("application/json")) {
      try {
        body = JSON.parse(text);
        const env = body as { error?: { code?: unknown; message?: unknown } };
        if (env && typeof env === "object" && env.error && typeof env.error === "object") {
          if (typeof env.error.code === "string") code = env.error.code;
          if (typeof env.error.message === "string") message = env.error.message;
        }
      } catch {
        /* keep text */
      }
    }
    const ra = headers.get("retry-after");
    const init: ConstructorParameters<typeof AurixError>[0] = { status, code, message, method, path, body };
    const rid = headers.get("x-request-id");
    if (rid) init.requestId = rid;
    if (ra) {
      const secs = Number(ra);
      if (Number.isFinite(secs)) init.retryAfterMs = Math.max(0, secs * 1000);
    }
    return new AurixError(init);
  }
}

/** Transport failure (DNS, connection refused, timeout, abort) — no HTTP response was received. */
export class AurixNetworkError extends Error {
  readonly method: string;
  readonly path: string;
  override readonly cause: unknown;

  constructor(message: string, method: string, path: string, cause?: unknown) {
    super(`${method} ${path}: ${message}`);
    this.name = "AurixNetworkError";
    this.method = method;
    this.path = path;
    this.cause = cause;
  }
}
