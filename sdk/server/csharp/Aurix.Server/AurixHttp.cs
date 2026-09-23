#nullable enable
using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Net;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading;
using System.Threading.Tasks;

namespace Aurix.Server;

/// <summary>
/// How requests authenticate. Set exactly one property; precedence when several are set:
/// ApiKey, AdminToken, PlayerToken, BootstrapToken.
/// </summary>
public sealed record Credentials
{
    /// <summary>Application API key (<c>X-API-Key</c>). Backend only — never ship it to game clients.</summary>
    public string? ApiKey { get; init; }
    /// <summary>Operator JWT from <c>POST /admin/login</c> (Bearer).</summary>
    public string? AdminToken { get; init; }
    /// <summary>Player session JWT (Bearer) for acting on behalf of a player.</summary>
    public string? PlayerToken { get; init; }
    /// <summary>Unlocks <c>POST /admin/setup</c> on a fresh deployment (<c>X-Bootstrap-Token</c>).</summary>
    public string? BootstrapToken { get; init; }

    internal void Apply(HttpRequestHeaders headers)
    {
        if (!string.IsNullOrEmpty(ApiKey)) headers.TryAddWithoutValidation("X-API-Key", ApiKey);
        else if (!string.IsNullOrEmpty(AdminToken)) headers.Authorization = new AuthenticationHeaderValue("Bearer", AdminToken);
        else if (!string.IsNullOrEmpty(PlayerToken)) headers.Authorization = new AuthenticationHeaderValue("Bearer", PlayerToken);
        else if (!string.IsNullOrEmpty(BootstrapToken)) headers.TryAddWithoutValidation("X-Bootstrap-Token", BootstrapToken);
    }
}

/// <summary>Client configuration.</summary>
public sealed record AurixClientOptions
{
    /// <summary>Node HTTP origin, e.g. <c>https://voice.example.com</c>.</summary>
    public required string BaseUrl { get; init; }
    public Credentials Credentials { get; init; } = new();
    /// <summary>Optional shared <see cref="HttpClient"/> (its <c>Timeout</c> is ignored; see <see cref="Timeout"/>).</summary>
    public HttpClient? HttpClient { get; init; }
    /// <summary>Per-request timeout (default 15 s).</summary>
    public TimeSpan Timeout { get; init; } = TimeSpan.FromSeconds(15);
    /// <summary>
    /// Automatic retries (default 2): idempotent methods on network errors and 502/503/504,
    /// every method on 429 (honouring <c>Retry-After</c>).
    /// </summary>
    public int MaxRetries { get; init; } = 2;
    public TimeSpan MaxBackoff { get; init; } = TimeSpan.FromSeconds(5);
    public IReadOnlyDictionary<string, string>? Headers { get; init; }
    public string? UserAgent { get; init; }
}

/// <summary>Per-call overrides.</summary>
public sealed record RequestOptions
{
    public IReadOnlyDictionary<string, string>? Headers { get; init; }
    public TimeSpan? Timeout { get; init; }
    /// <summary>Use different credentials for this call only.</summary>
    public Credentials? Credentials { get; init; }
}

/// <summary>Undecoded 2xx response for binary / CSV / SRT / VTT operations.</summary>
public sealed record RawResponse(int Status, string ContentType, IReadOnlyDictionary<string, string> Headers, byte[] Body)
{
    public string BodyText => Encoding.UTF8.GetString(Body);
}

/// <summary>
/// Hand-written transport under the generated <see cref="AurixClient"/>: auth headers, JSON,
/// retries, timeouts and error mapping.
/// </summary>
public abstract class AurixHttp : IDisposable
{
    public const string SdkVersion = "1.5.0";

    internal static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        PropertyNameCaseInsensitive = true,
        NumberHandling = JsonNumberHandling.AllowReadingFromString,
    };

    private static readonly HashSet<string> Idempotent = new(StringComparer.OrdinalIgnoreCase) { "GET", "HEAD", "OPTIONS", "PUT", "DELETE" };

    private readonly HttpClient _http;
    private readonly bool _ownsHttp;
    private readonly AurixClientOptions _options;
    private readonly Random _rng = new();

    protected AurixHttp(AurixClientOptions options)
    {
        if (string.IsNullOrWhiteSpace(options.BaseUrl)) throw new ArgumentException("BaseUrl is required", nameof(options));
        if (!Uri.TryCreate(options.BaseUrl, UriKind.Absolute, out _)) throw new ArgumentException("BaseUrl must be an absolute URL", nameof(options));
        _options = options;
        BaseUrl = options.BaseUrl.TrimEnd('/');
        _ownsHttp = options.HttpClient is null;
        _http = options.HttpClient ?? new HttpClient();
        if (_ownsHttp) _http.Timeout = System.Threading.Timeout.InfiniteTimeSpan;
        UserAgent = options.UserAgent ?? $"aurix-server-sdk-dotnet/{SdkVersion}";
    }

    public string BaseUrl { get; }
    public string UserAgent { get; }

    /// <summary>Builds an absolute URL for <paramref name="path"/> and <paramref name="query"/>.</summary>
    public string Url(string path, IReadOnlyDictionary<string, string>? query = null)
    {
        var sb = new StringBuilder(BaseUrl).Append(path);
        if (query is { Count: > 0 })
        {
            var first = true;
            foreach (var kv in query)
            {
                sb.Append(first ? '?' : '&').Append(Uri.EscapeDataString(kv.Key)).Append('=').Append(Uri.EscapeDataString(kv.Value));
                first = false;
            }
        }
        return sb.ToString();
    }

    protected async Task SendJsonAsync(string path, HttpMethod method, Dictionary<string, string>? query, object? body, RequestOptions? options, CancellationToken ct)
    {
        await SendRawAsync(path, method, query, body, options, ct, "application/json").ConfigureAwait(false);
    }

    protected async Task<T> SendJsonAsync<T>(string path, HttpMethod method, Dictionary<string, string>? query, object? body, RequestOptions? options, CancellationToken ct)
    {
        var raw = await SendRawAsync(path, method, query, body, options, ct, "application/json").ConfigureAwait(false);
        if (raw.Body.Length == 0 || raw.Status == 204)
        {
            throw new AurixException(raw.Status, "empty_response", $"expected a JSON body for {typeof(T).Name}", method.Method, path, null, null, raw.Body);
        }
        if (!raw.ContentType.StartsWith("application/json", StringComparison.OrdinalIgnoreCase))
        {
            throw new AurixException(raw.Status, "unexpected_content_type", $"expected application/json, got '{raw.ContentType}'; use the Raw variant", method.Method, path, null, null, raw.Body);
        }
        try
        {
            return JsonSerializer.Deserialize<T>(raw.Body, Json) ?? throw new AurixException(raw.Status, "invalid_json", "response was JSON null", method.Method, path, null, null, raw.Body);
        }
        catch (JsonException e)
        {
            throw new AurixException(raw.Status, "invalid_json", e.Message, method.Method, path, null, null, raw.Body);
        }
    }

    protected Task<RawResponse> SendRawAsync(string path, HttpMethod method, Dictionary<string, string>? query, object? body, RequestOptions? options, CancellationToken ct)
        => SendRawAsync(path, method, query, body, options, ct, "*/*");

    private async Task<RawResponse> SendRawAsync(string path, HttpMethod method, Dictionary<string, string>? query, object? body, RequestOptions? options, CancellationToken ct, string accept)
    {
        var url = Url(path, query);
        byte[]? payload = body is null ? null : JsonSerializer.SerializeToUtf8Bytes(body, body.GetType(), Json);
        var timeout = options?.Timeout ?? _options.Timeout;
        var idempotent = Idempotent.Contains(method.Method);
        for (var attempt = 0; ; attempt++)
        {
            using var req = new HttpRequestMessage(method, url);
            req.Headers.TryAddWithoutValidation("Accept", accept);
            req.Headers.TryAddWithoutValidation("User-Agent", UserAgent);
            if (_options.Headers is not null) foreach (var kv in _options.Headers) req.Headers.TryAddWithoutValidation(kv.Key, kv.Value);
            (options?.Credentials ?? _options.Credentials).Apply(req.Headers);
            if (options?.Headers is not null) foreach (var kv in options.Headers) req.Headers.TryAddWithoutValidation(kv.Key, kv.Value);
            if (payload is not null)
            {
                req.Content = new ByteArrayContent(payload);
                req.Content.Headers.ContentType = new MediaTypeHeaderValue("application/json");
            }

            using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
            cts.CancelAfter(timeout);
            HttpResponseMessage resp;
            byte[] data;
            try
            {
                resp = await _http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, cts.Token).ConfigureAwait(false);
                data = await resp.Content.ReadAsByteArrayAsync(cts.Token).ConfigureAwait(false);
            }
            catch (Exception e) when (e is HttpRequestException || e is IOException || (e is OperationCanceledException && !ct.IsCancellationRequested))
            {
                var err = e is OperationCanceledException ? new TimeoutException($"timeout after {timeout.TotalMilliseconds:0} ms", e) : e;
                if (attempt < _options.MaxRetries && idempotent)
                {
                    await Task.Delay(Backoff(attempt), ct).ConfigureAwait(false);
                    continue;
                }
                throw new AurixNetworkException(method.Method, path, err);
            }
            using (resp)
            {
                var status = (int)resp.StatusCode;
                var contentType = resp.Content.Headers.ContentType?.MediaType ?? "";
                var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
                foreach (var h in resp.Headers) headers[h.Key] = string.Join(",", h.Value);
                foreach (var h in resp.Content.Headers) headers[h.Key] = string.Join(",", h.Value);
                if (status >= 200 && status < 300)
                {
                    return new RawResponse(status, contentType, headers, data);
                }
                var retryAfter = ParseRetryAfter(headers);
                var retryable = status == 429 || ((status == 502 || status == 503 || status == 504) && idempotent);
                if (retryable && attempt < _options.MaxRetries)
                {
                    var delay = retryAfter ?? Backoff(attempt);
                    if (delay > _options.MaxBackoff) delay = _options.MaxBackoff;
                    await Task.Delay(delay, ct).ConfigureAwait(false);
                    continue;
                }
                throw AurixException.FromResponse(status, contentType, data, headers, method.Method, path, retryAfter);
            }
        }
    }

    internal static TimeSpan? ParseRetryAfter(IReadOnlyDictionary<string, string> headers)
    {
        if (!headers.TryGetValue("Retry-After", out var v)) return null;
        return double.TryParse(v, NumberStyles.Float, CultureInfo.InvariantCulture, out var secs) && secs >= 0 ? TimeSpan.FromSeconds(secs) : null;
    }

    private TimeSpan Backoff(int attempt)
    {
        var ms = 200.0 * Math.Pow(2, attempt);
        if (ms > _options.MaxBackoff.TotalMilliseconds) ms = _options.MaxBackoff.TotalMilliseconds;
        double jitter;
        lock (_rng) jitter = 0.5 + _rng.NextDouble() / 2;
        return TimeSpan.FromMilliseconds(ms * jitter);
    }

    /// <summary>Sends a GET for a long-lived stream (SSE) without the per-request timeout.</summary>
    internal async Task<HttpResponseMessage> OpenStreamAsync(string url, IReadOnlyDictionary<string, string>? extraHeaders, RequestOptions? options, CancellationToken ct)
    {
        var req = new HttpRequestMessage(HttpMethod.Get, url);
        req.Headers.TryAddWithoutValidation("Accept", "text/event-stream");
        req.Headers.TryAddWithoutValidation("User-Agent", UserAgent);
        if (_options.Headers is not null) foreach (var kv in _options.Headers) req.Headers.TryAddWithoutValidation(kv.Key, kv.Value);
        (options?.Credentials ?? _options.Credentials).Apply(req.Headers);
        if (options?.Headers is not null) foreach (var kv in options.Headers) req.Headers.TryAddWithoutValidation(kv.Key, kv.Value);
        if (extraHeaders is not null) foreach (var kv in extraHeaders) req.Headers.TryAddWithoutValidation(kv.Key, kv.Value);
        return await _http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, ct).ConfigureAwait(false);
    }

    public void Dispose()
    {
        if (_ownsHttp) _http.Dispose();
        GC.SuppressFinalize(this);
    }
}
