using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text.Json;
using Aurix.Examples.TokenServer;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging;
using Xunit;

namespace TokenServer.Tests;

/// <summary>Stand-in Aurix node: records every call, validates the API key, answers /v1/tokens.</summary>
sealed class FakeAurix : IAsyncDisposable
{
    public const string ApiKey = "aurx_test_SECRET_KEY_never_in_client_payload";
    public sealed record Call(string Path, string? ApiKey, JsonElement Body);
    public readonly List<Call> Calls = new();
    private readonly WebApplication _app;
    public string Url { get; private set; } = "";

    private FakeAurix()
    {
        var b = WebApplication.CreateBuilder();
        b.Logging.ClearProviders();
        b.WebHost.UseUrls("http://127.0.0.1:0");
        _app = b.Build();
        _app.Map("/{**path}", async (HttpContext ctx) =>
        {
            var body = await JsonDocument.ParseAsync(ctx.Request.Body);
            var key = ctx.Request.Headers["X-API-Key"].FirstOrDefault();
            lock (Calls) Calls.Add(new Call(ctx.Request.Path, key, body.RootElement.Clone()));
            if (ctx.Request.Path != "/v1/tokens")
                return Results.Json(new { error = new { code = "NOT_FOUND", message = "nope" } }, statusCode: 404);
            if (key != ApiKey)
                return Results.Json(new { error = new { code = "AUTH_FAILED", message = "bad key" } }, statusCode: 401);
            return Results.Json(new
            {
                token = "player.jwt",
                user_id = "u-1",
                expires_at = "2030-01-01T00:00:00Z",
                channels = Array.Empty<object>(),
                endpoint = new { region = "eu_west", node_id = "n1", ws_url = "wss://eu1.example/ws", nodes = 1, load_factor = 0.1 },
                api_key_echo = ApiKey,
            });
        });
    }

    public static async Task<FakeAurix> StartAsync()
    {
        var f = new FakeAurix();
        await f._app.StartAsync();
        f.Url = f._app.Urls.First();
        return f;
    }

    public async ValueTask DisposeAsync()
    {
        await _app.StopAsync();
        await _app.DisposeAsync();
    }
}

sealed class CapturingLoggerProvider : ILoggerProvider
{
    public readonly List<string> Lines = new();
    public ILogger CreateLogger(string categoryName) => new L(this);
    public void Dispose() { }

    private sealed class L(CapturingLoggerProvider p) : ILogger
    {
        public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;
        public bool IsEnabled(LogLevel logLevel) => true;
        public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception, Func<TState, Exception?, string> formatter)
        {
            lock (p.Lines) p.Lines.Add(formatter(state, exception));
        }
    }
}

public sealed class TokenServerTests
{
    private const string Secret = "0123456789abcdef0123456789abcdef";

    private static Config ConfigFor(FakeAurix aurix, string apiKey = FakeAurix.ApiKey) =>
        Config.Load(k => k switch
        {
            "AURIX_URL" => aurix.Url,
            "AURIX_API_KEY" => apiKey,
            "GAME_SESSION_SECRET" => Secret,
            "AURIX_REGION" => "eu_west",
            "ALLOW_DEV_LOGIN" => "1",
            _ => null,
        });

    private static WebApplicationFactory<Program> Factory(Config cfg, CapturingLoggerProvider? logs = null) =>
        new WebApplicationFactory<Program>().WithWebHostBuilder(b =>
        {
            b.ConfigureServices(s => s.AddSingleton(cfg));
            b.ConfigureLogging(l =>
            {
                l.ClearProviders();
                if (logs is not null) l.AddProvider(logs);
            });
        });

    private static async Task<(HttpStatusCode, JsonElement)> PostAsync(HttpClient http, string path, object body, string? session = null)
    {
        using var req = new HttpRequestMessage(HttpMethod.Post, path) { Content = JsonContent.Create(body) };
        if (session is not null) req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", session);
        using var res = await http.SendAsync(req);
        var json = await JsonDocument.ParseAsync(await res.Content.ReadAsStreamAsync());
        return (res.StatusCode, json.RootElement.Clone());
    }

    [Fact]
    public void ConfigRefusesUnsafeStartup()
    {
        static Func<string, string?> Env(Dictionary<string, string> m) => k => m.GetValueOrDefault(k);
        Assert.Throws<InvalidOperationException>(() => Config.Load(Env(new() { ["GAME_SESSION_SECRET"] = Secret })));
        Assert.Throws<InvalidOperationException>(() => Config.Load(Env(new() { ["AURIX_API_KEY"] = "k", ["GAME_SESSION_SECRET"] = "short" })));
        Assert.Throws<InvalidOperationException>(() => Config.Load(Env(new() { ["AURIX_API_KEY"] = "k", ["GAME_SESSION_SECRET"] = Secret, ["AURIX_REGION"] = "eu" })));
    }

    [Fact]
    public void GameSessionRejectsForgedAndExpired()
    {
        var good = GameSession.MintDev(Secret, "p1", "Alice", TimeSpan.FromHours(1));
        Assert.Equal(new Player("p1", "Alice"), GameSession.Authenticate(Secret, "Bearer " + good));
        Assert.Null(GameSession.Authenticate(new string('x', 32), "Bearer " + good));
        Assert.Null(GameSession.Authenticate(Secret, "Bearer " + good.Split('.')[0] + ".AAAA"));
        Assert.Null(GameSession.Authenticate(Secret, "Bearer " + GameSession.MintDev(Secret, "p1", "Alice", TimeSpan.FromSeconds(-1))));
        Assert.Null(GameSession.Authenticate(Secret, null));
    }

    [Fact]
    public async Task TokenRequiresSession()
    {
        await using var aurix = await FakeAurix.StartAsync();
        await using var factory = Factory(ConfigFor(aurix));
        var (status, _) = await PostAsync(factory.CreateClient(), "/voice/token", new { match_id = "m1", external_id = "admin" });
        Assert.Equal(HttpStatusCode.Unauthorized, status);
        Assert.Empty(aurix.Calls);
    }

    [Fact]
    public async Task HappyPathHidesApiKey()
    {
        await using var aurix = await FakeAurix.StartAsync();
        await using var factory = Factory(ConfigFor(aurix));
        var http = factory.CreateClient();
        var (loginStatus, login) = await PostAsync(http, "/dev/login", new { player_id = "p1", display_name = "Alice" });
        Assert.Equal(HttpStatusCode.OK, loginStatus);

        var (status, body) = await PostAsync(http, "/voice/token",
            new { match_id = "m1", external_id = "spoof", channels = new[] { "*" } }, login.GetProperty("session").GetString());
        Assert.Equal(HttpStatusCode.OK, status);
        var raw = body.GetRawText();
        Assert.DoesNotContain(FakeAurix.ApiKey, raw);
        Assert.Equal(new[] { "endpoint", "expires_at", "token", "user_id" }, body.EnumerateObject().Select(p => p.Name).OrderBy(n => n));
        Assert.Equal("player.jwt", body.GetProperty("token").GetString());
        Assert.Equal("u-1", body.GetProperty("user_id").GetString());
        Assert.Equal("2030-01-01T00:00:00Z", body.GetProperty("expires_at").GetString());
        Assert.Equal("wss://eu1.example/ws", body.GetProperty("endpoint").GetProperty("ws_url").GetString());
        Assert.Equal("eu_west", body.GetProperty("endpoint").GetProperty("region").GetString());
        Assert.Equal(new[] { "region", "ws_url" }, body.GetProperty("endpoint").EnumerateObject().Select(p => p.Name).OrderBy(n => n));

        var call = Assert.Single(aurix.Calls);
        Assert.Equal("/v1/tokens", call.Path);
        Assert.Equal(FakeAurix.ApiKey, call.ApiKey);
        Assert.Equal("p1", call.Body.GetProperty("external_id").GetString());
        Assert.Equal("Alice", call.Body.GetProperty("display_name").GetString());
        Assert.Equal("eu_west", call.Body.GetProperty("region").GetString());
        var grant = Assert.Single(call.Body.GetProperty("channels").EnumerateArray());
        Assert.Equal(new[] { "ad_hoc", "join", "receive", "speak" }, grant.EnumerateObject().Select(p => p.Name).OrderBy(n => n));
        Assert.Equal("match-m1", grant.GetProperty("ad_hoc").GetProperty("name").GetString());
        Assert.Equal("team", grant.GetProperty("ad_hoc").GetProperty("channel_type").GetString());
        Assert.True(grant.GetProperty("join").GetBoolean() && grant.GetProperty("speak").GetBoolean() && grant.GetProperty("receive").GetBoolean());
    }

    [Fact]
    public async Task InvalidMatchRefusedBeforeAurix()
    {
        await using var aurix = await FakeAurix.StartAsync();
        await using var factory = Factory(ConfigFor(aurix));
        var (status, _) = await PostAsync(factory.CreateClient(), "/voice/token", new { match_id = "../etc" },
            GameSession.MintDev(Secret, "p1", "Alice", TimeSpan.FromHours(1)));
        Assert.Equal(HttpStatusCode.Forbidden, status);
        Assert.Empty(aurix.Calls);
    }

    [Fact]
    public async Task AurixErrorIsGenericAndLogged()
    {
        await using var aurix = await FakeAurix.StartAsync();
        var logs = new CapturingLoggerProvider();
        await using var factory = Factory(ConfigFor(aurix, apiKey: "aurx_wrong_key"), logs);
        var (status, body) = await PostAsync(factory.CreateClient(), "/voice/token", new { match_id = "m1" },
            GameSession.MintDev(Secret, "p1", "A", TimeSpan.FromHours(1)));
        Assert.Equal(HttpStatusCode.BadGateway, status);
        Assert.Equal("voice service unavailable", body.GetProperty("error").GetString());
        Assert.Contains(logs.Lines, l => l.Contains("aurix 401 AUTH_FAILED"));
        Assert.DoesNotContain(logs.Lines, l => l.Contains("aurx_wrong_key"));
    }
}
