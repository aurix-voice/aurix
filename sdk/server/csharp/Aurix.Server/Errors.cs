#nullable enable
using System;
using System.Collections.Generic;
using System.Text;
using System.Text.Json;

namespace Aurix.Server;

/// <summary>
/// A non-2xx response. <see cref="Code"/> and <see cref="Detail"/> come from the API's
/// <c>{"error": {"code", "message"}}</c> envelope when present.
/// </summary>
public sealed class AurixException : Exception
{
    public AurixException(int status, string code, string message, string method, string path, string? requestId, TimeSpan? retryAfter, byte[] body)
        : base($"{method} {path} -> {status} {code}: {message}")
    {
        Status = status;
        Code = code;
        Detail = message;
        Method = method;
        Path = path;
        RequestId = requestId;
        RetryAfter = retryAfter;
        Body = body;
    }

    public int Status { get; }
    public string Code { get; }
    /// <summary>The API's message without the method/path prefix.</summary>
    public string Detail { get; }
    public string Method { get; }
    public string Path { get; }
    public string? RequestId { get; }
    public TimeSpan? RetryAfter { get; }
    /// <summary>Raw response body.</summary>
    public byte[] Body { get; }

    public bool IsAuth => Status == 401 || Status == 403;
    public bool IsNotFound => Status == 404;
    public bool IsRateLimited => Status == 429;

    internal static AurixException FromResponse(int status, string contentType, byte[] body, IReadOnlyDictionary<string, string> headers, string method, string path, TimeSpan? retryAfter)
    {
        var code = $"http_{status}";
        var message = body.Length > 0 ? Encoding.UTF8.GetString(body, 0, Math.Min(body.Length, 512)) : $"HTTP {status}";
        if (contentType.StartsWith("application/json", StringComparison.OrdinalIgnoreCase))
        {
            try
            {
                using var doc = JsonDocument.Parse(body);
                if (doc.RootElement.ValueKind == JsonValueKind.Object && doc.RootElement.TryGetProperty("error", out var err) && err.ValueKind == JsonValueKind.Object)
                {
                    if (err.TryGetProperty("code", out var c) && c.ValueKind == JsonValueKind.String) code = c.GetString() ?? code;
                    if (err.TryGetProperty("message", out var m) && m.ValueKind == JsonValueKind.String) message = m.GetString() ?? message;
                }
            }
            catch (JsonException)
            {
                // keep the raw text
            }
        }
        headers.TryGetValue("X-Request-Id", out var requestId);
        return new AurixException(status, code, message, method, path, requestId, retryAfter, body);
    }
}

/// <summary>Transport failure (DNS, refused connection, timeout) — no HTTP response was received.</summary>
public sealed class AurixNetworkException : Exception
{
    public AurixNetworkException(string method, string path, Exception inner) : base($"{method} {path}: {inner.Message}", inner)
    {
        Method = method;
        Path = path;
    }

    public string Method { get; }
    public string Path { get; }
}
