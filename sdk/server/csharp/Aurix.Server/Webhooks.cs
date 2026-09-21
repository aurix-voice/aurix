#nullable enable
using System;
using System.Collections.Generic;
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Aurix.Server;

/// <summary>One verified webhook POST.</summary>
public sealed record IncomingWebhook(EventEnvelope Event, string? WebhookId, string? DeliveryId, int Attempt);

/// <summary>
/// Webhook signature helpers. Header format:
/// <c>X-Aurix-Signature: t=&lt;unix seconds&gt;,v1=&lt;hex(HMAC-SHA256(secret, "&lt;t&gt;.&lt;raw body&gt;"))&gt;</c>.
/// Always verify the exact bytes received, never a re-serialised object.
/// </summary>
public static class Webhooks
{
    public const string SignatureHeader = "X-Aurix-Signature";
    public const string EventHeader = "X-Aurix-Event";
    public const string WebhookIdHeader = "X-Aurix-Webhook-Id";
    public const string DeliveryIdHeader = "X-Aurix-Delivery-Id";
    public const string AttemptHeader = "X-Aurix-Attempt";
    public static readonly TimeSpan DefaultTolerance = TimeSpan.FromMinutes(5);

    /// <summary>Computes the signature header value for <paramref name="body"/> at <paramref name="unixSeconds"/>.</summary>
    public static string Sign(string secret, long unixSeconds, ReadOnlySpan<byte> body)
    {
        var ts = unixSeconds.ToString(CultureInfo.InvariantCulture);
        var prefix = Encoding.ASCII.GetBytes(ts + ".");
        var input = new byte[prefix.Length + body.Length];
        prefix.CopyTo(input, 0);
        body.CopyTo(input.AsSpan(prefix.Length));
        using var mac = new HMACSHA256(Encoding.UTF8.GetBytes(secret));
        return $"t={ts},v1={Convert.ToHexString(mac.ComputeHash(input)).ToLowerInvariant()}";
    }

    public static string Sign(string secret, long unixSeconds, string body) => Sign(secret, unixSeconds, Encoding.UTF8.GetBytes(body));

    /// <summary>Constant-time verification of <paramref name="header"/> against the raw body within the replay window.</summary>
    public static bool Verify(string secret, string? header, ReadOnlySpan<byte> body, TimeSpan? tolerance = null, DateTimeOffset? now = null)
    {
        if (string.IsNullOrEmpty(secret) || string.IsNullOrEmpty(header)) return false;
        long? ts = null;
        string? v1 = null;
        foreach (var part in header.Split(','))
        {
            var eq = part.IndexOf('=');
            if (eq < 0) continue;
            var k = part[..eq].Trim();
            var v = part[(eq + 1)..].Trim();
            if (k == "t" && long.TryParse(v, NumberStyles.None, CultureInfo.InvariantCulture, out var n)) ts = n;
            else if (k == "v1") v1 = v.ToLowerInvariant();
        }
        if (ts is null || string.IsNullOrEmpty(v1)) return false;
        var window = (long)(tolerance ?? DefaultTolerance).TotalSeconds;
        var nowSec = (now ?? DateTimeOffset.UtcNow).ToUnixTimeSeconds();
        if (Math.Abs(nowSec - ts.Value) > window) return false;
        var expected = Sign(secret, ts.Value, body);
        var expectedHex = expected[(expected.IndexOf("v1=", StringComparison.Ordinal) + 3)..];
        byte[] got;
        try { got = Convert.FromHexString(v1); }
        catch (FormatException) { return false; }
        return CryptographicOperations.FixedTimeEquals(got, Convert.FromHexString(expectedHex));
    }

    public static bool Verify(string secret, string? header, string body, TimeSpan? tolerance = null, DateTimeOffset? now = null)
        => Verify(secret, header, Encoding.UTF8.GetBytes(body), tolerance, now);

    /// <summary>
    /// Verifies and decodes one webhook request. <paramref name="headers"/> is matched case-insensitively;
    /// <paramref name="rawBody"/> must be the exact bytes received.
    /// </summary>
    public static IncomingWebhook Parse(string secret, IReadOnlyDictionary<string, string> headers, ReadOnlySpan<byte> rawBody, TimeSpan? tolerance = null, DateTimeOffset? now = null)
    {
        string? Get(string name)
        {
            foreach (var kv in headers) if (string.Equals(kv.Key, name, StringComparison.OrdinalIgnoreCase)) return kv.Value;
            return null;
        }
        if (!Verify(secret, Get(SignatureHeader), rawBody, tolerance, now))
        {
            throw new WebhookSignatureException("invalid or expired X-Aurix-Signature");
        }
        var env = JsonSerializer.Deserialize<EventEnvelope>(rawBody, AurixHttp.Json) ?? throw new WebhookSignatureException("empty webhook body");
        var type = Get(EventHeader);
        if (type is not null && type != env.Type)
        {
            throw new WebhookSignatureException($"{EventHeader} '{type}' does not match body type '{env.Type}'");
        }
        var attempt = int.TryParse(Get(AttemptHeader), NumberStyles.None, CultureInfo.InvariantCulture, out var a) && a > 0 ? a : 1;
        return new IncomingWebhook(env, Get(WebhookIdHeader), Get(DeliveryIdHeader), attempt);
    }
}

/// <summary>Thrown by <see cref="Webhooks.Parse"/> for missing, stale or mismatching signatures.</summary>
public sealed class WebhookSignatureException : Exception
{
    public WebhookSignatureException(string message) : base(message) { }
}
