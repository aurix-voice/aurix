using System.Net;
using System.Text;
using System.Text.Json;
using Xunit;

namespace Aurix.Server.Tests;

/// <summary>Minimal scripted HTTP server standing in for an Aurix node.</summary>
sealed class FakeNode : IDisposable
{
    public sealed record Call(string Method, string Path, Dictionary<string, string> Query, Dictionary<string, string> Headers, JsonElement? Body);

    private readonly HttpListener _listener = new();
    private readonly Func<HttpListenerContext, int, Task> _handler;
    private readonly CancellationTokenSource _cts = new();
    public readonly List<Call> Calls = new();
    public string BaseUrl { get; }

    public FakeNode(Func<HttpListenerContext, int, Task> handler)
    {
        _handler = handler;
        var port = FreePort();
        BaseUrl = $"http://127.0.0.1:{port}";
        _listener.Prefixes.Add(BaseUrl + "/");
        _listener.Start();
        _ = Task.Run(LoopAsync);
    }

    private static int FreePort()
    {
        var l = new System.Net.Sockets.TcpListener(IPAddress.Loopback, 0);
        l.Start();
        var port = ((IPEndPoint)l.LocalEndpoint).Port;
        l.Stop();
        return port;
    }

    private async Task LoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            HttpListenerContext ctx;
            try { ctx = await _listener.GetContextAsync(); }
            catch (Exception) when (_cts.IsCancellationRequested) { return; }
            catch (HttpListenerException) { return; }
            _ = Task.Run(async () =>
            {
                using var ms = new MemoryStream();
                await ctx.Request.InputStream.CopyToAsync(ms);
                var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
                foreach (string k in ctx.Request.Headers) headers[k] = ctx.Request.Headers[k]!;
                var query = new Dictionary<string, string>();
                foreach (string k in ctx.Request.QueryString) if (k is not null) query[k] = ctx.Request.QueryString[k]!;
                JsonElement? body = ms.Length > 0 ? JsonDocument.Parse(ms.ToArray()).RootElement.Clone() : null;
                int n;
                lock (Calls)
                {
                    Calls.Add(new Call(ctx.Request.HttpMethod, ctx.Request.Url!.AbsolutePath, query, headers, body));
                    n = Calls.Count;
                }
                try { await _handler(ctx, n); }
                catch (Exception) { /* client went away */ }
                try { ctx.Response.Close(); } catch (Exception) { }
            });
        }
    }

    public static Task Respond(HttpListenerContext ctx, int status, object body, string contentType = "application/json", Dictionary<string, string>? headers = null)
    {
        var bytes = contentType == "application/json" ? JsonSerializer.SerializeToUtf8Bytes(body) : Encoding.UTF8.GetBytes((string)body);
        ctx.Response.StatusCode = status;
        ctx.Response.ContentType = contentType;
        if (headers is not null) foreach (var kv in headers) ctx.Response.Headers[kv.Key] = kv.Value;
        return ctx.Response.OutputStream.WriteAsync(bytes).AsTask();
    }

    public void Dispose()
    {
        _cts.Cancel();
        _listener.Stop();
        _listener.Close();
    }
}

public class ClientTests
{
    private static readonly object TokenBody = new
    {
        token = "jwt", user_id = "11111111-0000-4000-8000-000000000001", expires_at = "2030-01-01T00:00:00Z", channels = Array.Empty<object>(),
        endpoint = new { region = "eu_west", node_id = "22222222-0000-4000-8000-000000000002", ws_url = "wss://node/ws", probe_url = (string?)null, location = (object?)null, distance_km = (double?)null, nodes = 1, load_factor = 0.1 },
    };

    private static AurixClient Client(FakeNode n, Credentials? creds = null, int? maxRetries = null) => new(new AurixClientOptions
    {
        BaseUrl = n.BaseUrl + "/",
        Credentials = creds ?? new Credentials { ApiKey = "ak_test" },
        MaxBackoff = TimeSpan.FromMilliseconds(20),
        MaxRetries = maxRetries ?? 2,
    });

    [Fact]
    public async Task IssueTokenSendsApiKeyAndBody()
    {
        using var n = new FakeNode((c, _) => FakeNode.Respond(c, 200, TokenBody));
        using var client = Client(n);
        var tok = await client.IssueTokenAsync(new GenerateTokenRequest { ExternalId = "player-1", DisplayName = "Player", Region = Region.EuWest });
        Assert.Equal("jwt", tok.Token);
        Assert.Equal("wss://node/ws", tok.Endpoint!.WsUrl);
        var call = n.Calls[0];
        Assert.Equal("POST", call.Method);
        Assert.Equal("/v1/tokens", call.Path);
        Assert.Equal("ak_test", call.Headers["X-API-Key"]);
        Assert.False(call.Headers.ContainsKey("Authorization"));
        Assert.StartsWith("aurix-server-sdk-dotnet/", call.Headers["User-Agent"]);
        Assert.Equal("player-1", call.Body!.Value.GetProperty("external_id").GetString());
        Assert.Equal("eu_west", call.Body!.Value.GetProperty("region").GetString());
        Assert.False(call.Body!.Value.TryGetProperty("channels", out _), "omitted optional fields must not be serialised");
    }

    [Fact]
    public async Task PathAndQueryEncoding()
    {
        using var n = new FakeNode((c, _) => FakeNode.Respond(c, 200, new { messages = Array.Empty<object>(), next_before = (string?)null, next_after = (string?)null }));
        using var client = Client(n);
        await client.ListChannelMessagesAsync("ch/1 x", new ListChannelMessagesQuery { Limit = 10, Before = "a/b c" });
        var call = n.Calls[0];
        Assert.Equal("/v1/channels/ch%2F1%20x/messages", call.Path);
        Assert.Equal("10", call.Query["limit"]);
        Assert.Equal("a/b c", call.Query["before"]);
    }

    [Fact]
    public async Task BearerAuthAndPerCallOverride()
    {
        using var n = new FakeNode((c, _) => FakeNode.Respond(c, 200, new { status = "healthy", version = "1", node_id = "22222222-0000-4000-8000-000000000002", timestamp = "2030-01-01T00:00:00Z", active_sessions = 0, active_channels = 0 }));
        using var client = Client(n, new Credentials { AdminToken = "adm" });
        await client.HealthAsync();
        await client.HealthAsync(new RequestOptions { Credentials = new Credentials { PlayerToken = "ply" }, Headers = new Dictionary<string, string> { ["X-Trace"] = "1" } });
        Assert.Equal("Bearer adm", n.Calls[0].Headers["Authorization"]);
        Assert.Equal("Bearer ply", n.Calls[1].Headers["Authorization"]);
        Assert.Equal("1", n.Calls[1].Headers["X-Trace"]);
    }

    [Fact]
    public async Task ErrorEnvelope()
    {
        using var n = new FakeNode((c, _) => FakeNode.Respond(c, 404, new { error = new { code = "NOT_FOUND", message = "channel missing" } }, headers: new() { ["X-Request-Id"] = "req-1" }));
        using var client = Client(n);
        var e = await Assert.ThrowsAsync<AurixException>(() => client.GetChannelAsync("x"));
        Assert.Equal(404, e.Status);
        Assert.Equal("NOT_FOUND", e.Code);
        Assert.Equal("channel missing", e.Detail);
        Assert.Equal("req-1", e.RequestId);
        Assert.True(e.IsNotFound);
    }

    [Fact]
    public async Task RetriesIdempotentAndRateLimited()
    {
        using var n = new FakeNode((c, i) => i switch
        {
            1 => FakeNode.Respond(c, 503, "", "text/plain"),
            2 => FakeNode.Respond(c, 429, "", "text/plain", new() { ["Retry-After"] = "0" }),
            _ => FakeNode.Respond(c, 200, new { page = 1, per_page = 20, total = 0, data = Array.Empty<object>() }),
        });
        using var client = Client(n);
        await client.ListChannelsAsync();
        Assert.Equal(3, n.Calls.Count);
    }

    [Fact]
    public async Task NoRetryForPostOn503()
    {
        using var n = new FakeNode((c, _) => FakeNode.Respond(c, 503, "", "text/plain"));
        using var client = Client(n);
        var e = await Assert.ThrowsAsync<AurixException>(() => client.IssueTokenAsync(new GenerateTokenRequest { ExternalId = "u", DisplayName = "U" }));
        Assert.Equal(503, e.Status);
        Assert.Single(n.Calls);
    }

    [Fact]
    public async Task TimeoutIsNetworkException()
    {
        using var n = new FakeNode(async (c, _) => { await Task.Delay(1500); await FakeNode.Respond(c, 200, new { }); });
        using var client = new AurixClient(new AurixClientOptions { BaseUrl = n.BaseUrl, Credentials = new Credentials { ApiKey = "k" }, Timeout = TimeSpan.FromMilliseconds(50), MaxRetries = 0 });
        var e = await Assert.ThrowsAsync<AurixNetworkException>(() => client.HealthAsync());
        Assert.IsType<TimeoutException>(e.InnerException);
    }

    [Fact]
    public async Task RawAndTypedContentType()
    {
        using var n = new FakeNode((c, _) => FakeNode.Respond(c, 200, "a,b\n1,2\n", "text/csv; charset=utf-8"));
        using var client = Client(n);
        var raw = await client.ExportUsageRawAsync();
        Assert.Equal("text/csv", raw.ContentType);
        Assert.Equal("a,b\n1,2\n", raw.BodyText);
        Assert.Equal("*/*", n.Calls[0].Headers["Accept"]);
        var e = await Assert.ThrowsAsync<AurixException>(() => client.ExportUsageAsync());
        Assert.Equal("unexpected_content_type", e.Code);
    }

    [Fact]
    public async Task NoContent()
    {
        using var n = new FakeNode((c, _) => { c.Response.StatusCode = 204; return Task.CompletedTask; });
        using var client = Client(n);
        await client.DeleteWebhookAsync("w1");
        Assert.Equal("DELETE", n.Calls[0].Method);
        Assert.Equal("/v1/webhooks/w1", n.Calls[0].Path);
    }

    [Fact]
    public void WebhookSignatureVector()
    {
        var v = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(AppContext.BaseDirectory, "vectors", "webhook_signature.json"))).RootElement;
        var secret = v.GetProperty("secret").GetString()!;
        var header = v.GetProperty("header").GetString()!;
        var body = v.GetProperty("body").GetString()!;
        var t = v.GetProperty("timestamp").GetInt64();
        var tolerance = TimeSpan.FromSeconds(v.GetProperty("tolerance_sec").GetInt64());
        DateTimeOffset At(long off) => DateTimeOffset.FromUnixTimeSeconds(t + off);

        Assert.Equal(header, Webhooks.Sign(secret, t, body));
        Assert.True(Webhooks.Verify(secret, header, body, tolerance, At(10)));
        Assert.False(Webhooks.Verify(secret, header, body + " ", tolerance, At(10)));
        Assert.False(Webhooks.Verify("other", header, body, tolerance, At(10)));
        Assert.False(Webhooks.Verify(secret, header, body, tolerance, At((long)tolerance.TotalSeconds + 1)));
        Assert.False(Webhooks.Verify(secret, "v1=abc", body, tolerance, At(0)));
        Assert.False(Webhooks.Verify(secret, null, body, tolerance, At(0)));

        var headers = new Dictionary<string, string> { ["x-aurix-signature"] = header, ["X-Aurix-Event"] = "participant.joined", ["X-Aurix-Delivery-Id"] = "d1", ["X-Aurix-Attempt"] = "2" };
        var d = Webhooks.Parse(secret, headers, Encoding.UTF8.GetBytes(body), tolerance, At(0));
        Assert.Equal("participant.joined", d.Event.Type);
        Assert.Equal("d1", d.DeliveryId);
        Assert.Equal(2, d.Attempt);
        headers["X-Aurix-Event"] = "participant.left";
        Assert.Throws<WebhookSignatureException>(() => Webhooks.Parse(secret, headers, Encoding.UTF8.GetBytes(body), tolerance, At(0)));
    }

    [Fact]
    public async Task SseParser()
    {
        var raw = ": keepalive\n\nevent: stream.open\ndata: {\"ok\":true}\n\nid: 42\r\nevent: participant.joined\r\ndata: {\"id\":\"e1\",\r\ndata: \"type\":\"x\"}\r\n\r\nretry: 250\ndata: tail\n\n";
        var events = new List<SseParser.Raw>();
        await foreach (var e in Server.SseParser.ReadAsync(new MemoryStream(Encoding.UTF8.GetBytes(raw)))) events.Add(e);
        Assert.Equal(3, events.Count);
        Assert.Equal(("stream.open", "{\"ok\":true}"), (events[0].Type, events[0].Data));
        Assert.Equal("42", events[1].Id);
        Assert.Equal("{\"id\":\"e1\",\n\"type\":\"x\"}", events[1].Data);
        Assert.Equal(250, events[2].Retry);
        Assert.Null(events[2].Type);
    }

    [Fact]
    public async Task EventStreamReconnectsWithLastEventId()
    {
        using var n = new FakeNode(async (c, i) =>
        {
            c.Response.StatusCode = 200;
            c.Response.ContentType = "text/event-stream";
            c.Response.SendChunked = true;
            if (i == 1)
            {
                await c.Response.OutputStream.WriteAsync(Encoding.UTF8.GetBytes("event: stream.open\ndata: {}\n\nid: ev-1\nevent: participant.joined\ndata: {\"id\":\"ev-1\",\"type\":\"participant.joined\"}\n\n"));
                return;
            }
            await c.Response.OutputStream.WriteAsync(Encoding.UTF8.GetBytes("id: ev-2\nevent: participant.left\ndata: {\"id\":\"ev-2\",\"type\":\"participant.left\"}\n\n"));
            await c.Response.OutputStream.FlushAsync();
            await Task.Delay(2000);
        });
        using var client = Client(n);
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        var seen = new List<string>();
        try
        {
            await foreach (var ev in client.EventsAsync(new EventStreamOptions { Types = new[] { "participant.joined", "participant.left" }, ReconnectDelay = TimeSpan.FromMilliseconds(5) }, cts.Token))
            {
                seen.Add(ev.Type);
                if (seen.Count == 3) cts.Cancel();
            }
        }
        catch (OperationCanceledException) { }
        Assert.Equal(new[] { "stream.open", "participant.joined", "participant.left" }, seen);
        Assert.Equal("participant.joined,participant.left", n.Calls[0].Query["types"]);
        Assert.Equal("text/event-stream", n.Calls[0].Headers["Accept"]);
        Assert.Equal("ev-1", n.Calls[1].Headers["Last-Event-ID"]);
    }
}
