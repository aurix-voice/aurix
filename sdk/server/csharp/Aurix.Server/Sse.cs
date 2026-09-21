#nullable enable
using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace Aurix.Server;

/// <summary>One message from <c>GET /v1/events</c>.</summary>
public sealed record SseEvent(string Type, string? Id, string Data)
{
    /// <summary>Decodes <see cref="Data"/> as an <see cref="EventEnvelope"/> (not for <c>stream.open</c> / <c>lagged</c>).</summary>
    public EventEnvelope? Envelope()
    {
        try { return JsonSerializer.Deserialize<EventEnvelope>(Data, AurixHttp.Json); }
        catch (JsonException) { return null; }
    }
}

/// <summary>Options for <see cref="AurixClient.EventsAsync"/>.</summary>
public sealed record EventStreamOptions
{
    /// <summary>Event types to receive (server default: everything except high-frequency events).</summary>
    public IReadOnlyList<string>? Types { get; init; }
    /// <summary>Reconnect after the stream ends or fails (default true).</summary>
    public bool Reconnect { get; init; } = true;
    public TimeSpan ReconnectDelay { get; init; } = TimeSpan.FromSeconds(1);
    public TimeSpan MaxReconnectDelay { get; init; } = TimeSpan.FromSeconds(30);
    /// <summary>Resume from a known position on the first connection.</summary>
    public string? LastEventId { get; init; }
    public RequestOptions? Request { get; init; }
}

public sealed partial class AurixClient
{
    /// <summary>
    /// Streams the application's events until <paramref name="ct"/> is cancelled or a non-retryable
    /// HTTP error occurs. Reconnects with <c>Last-Event-ID</c>; on a <c>lagged</c> event fetch
    /// <c>/v1/events/snapshot</c> to resynchronise.
    /// </summary>
    public async IAsyncEnumerable<SseEvent> EventsAsync(EventStreamOptions? options = null, [EnumeratorCancellation] CancellationToken ct = default)
    {
        options ??= new EventStreamOptions();
        var query = new Dictionary<string, string>();
        if (options.Types is { Count: > 0 }) query["types"] = string.Join(",", options.Types);
        var url = Url("/v1/events", query);
        var delay = options.ReconnectDelay;
        var lastId = options.LastEventId;
        while (true)
        {
            ct.ThrowIfCancellationRequested();
            HttpResponseMessage resp;
            try
            {
                var extra = lastId is null ? null : new Dictionary<string, string> { ["Last-Event-ID"] = lastId };
                resp = await OpenStreamAsync(url, extra, options.Request, ct).ConfigureAwait(false);
            }
            catch (Exception e) when (e is HttpRequestException || e is IOException)
            {
                if (!options.Reconnect) throw new AurixNetworkException("GET", "/v1/events", e);
                await Task.Delay(delay, ct).ConfigureAwait(false);
                delay = Min(options.MaxReconnectDelay, delay * 2);
                continue;
            }
            using (resp)
            {
                var status = (int)resp.StatusCode;
                if (status < 200 || status >= 300)
                {
                    var body = await resp.Content.ReadAsByteArrayAsync(ct).ConfigureAwait(false);
                    var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
                    foreach (var h in resp.Headers) headers[h.Key] = string.Join(",", h.Value);
                    var err = AurixException.FromResponse(status, resp.Content.Headers.ContentType?.MediaType ?? "", body, headers, "GET", "/v1/events", AurixHttp.ParseRetryAfter(headers));
                    if (options.Reconnect && (status == 429 || status >= 500))
                    {
                        await Task.Delay(err.RetryAfter ?? delay, ct).ConfigureAwait(false);
                        delay = Min(options.MaxReconnectDelay, delay * 2);
                        continue;
                    }
                    throw err;
                }
                delay = options.ReconnectDelay;
                Stream stream;
                try { stream = await resp.Content.ReadAsStreamAsync(ct).ConfigureAwait(false); }
                catch (Exception e) when (e is HttpRequestException || e is IOException) { if (!options.Reconnect) throw new AurixNetworkException("GET", "/v1/events", e); continue; }
                await foreach (var raw in SseParser.ReadAsync(stream, ct).ConfigureAwait(false))
                {
                    if (raw.Id is not null) lastId = raw.Id;
                    if (raw.Retry is { } ms) delay = TimeSpan.FromMilliseconds(ms);
                    yield return new SseEvent(raw.Type ?? "message", raw.Id, raw.Data);
                }
            }
            if (!options.Reconnect) yield break;
            await Task.Delay(delay, ct).ConfigureAwait(false);
            delay = Min(options.MaxReconnectDelay, delay * 2);
        }
    }

    private static TimeSpan Min(TimeSpan a, TimeSpan b) => a < b ? a : b;
}

/// <summary>text/event-stream parser (comments, multi-line data, id, event, retry; LF/CRLF/CR).</summary>
public static class SseParser
{
    public sealed record Raw(string? Type, string? Id, string Data, int? Retry);

    public static async IAsyncEnumerable<Raw> ReadAsync(Stream stream, [EnumeratorCancellation] CancellationToken ct = default)
    {
        using var reader = new StreamReader(stream, new UTF8Encoding(false), false, 16 * 1024, leaveOpen: true);
        string? type = null, id = null;
        int? retry = null;
        var data = new List<string>();
        while (true)
        {
            string? line;
            try { line = await reader.ReadLineAsync(ct).ConfigureAwait(false); }
            catch (Exception e) when (e is HttpRequestException || e is IOException) { break; }
            if (line is null) break;
            if (line.Length == 0)
            {
                if (data.Count > 0 || type is not null || id is not null)
                {
                    yield return new Raw(type, id, string.Join("\n", data), retry);
                }
                type = null; id = null; retry = null; data.Clear();
                continue;
            }
            if (line[0] == ':') continue;
            var colon = line.IndexOf(':');
            var field = colon < 0 ? line : line[..colon];
            var value = colon < 0 ? "" : line[(colon + 1)..];
            if (value.StartsWith(' ')) value = value[1..];
            switch (field)
            {
                case "event": type = value; break;
                case "data": data.Add(value); break;
                case "id": if (!value.Contains('\0')) id = value; break;
                case "retry": if (int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var n)) retry = n; break;
            }
        }
    }
}
