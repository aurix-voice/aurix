// Backend-only Aurix token server (ASP.NET Core minimal API).
//
//   game client --(game session)--> POST /voice/token --> this server --(API key)--> Aurix POST /v1/tokens
//   game client <-- { token, user_id, expires_at, endpoint } <---------------------------'
//
// The API key lives only in this process' environment. Clients never see it, never choose their own
// player id (that comes from the game session) and never choose their grants.

using System.Text.Json.Serialization;
using Aurix.Examples.TokenServer;
using Aurix.Server;

var builder = WebApplication.CreateBuilder(args);
builder.WebHost.ConfigureKestrel(o => o.Limits.MaxRequestBodySize = 4096);
builder.Services.AddSingleton(_ => Config.Load(Environment.GetEnvironmentVariable));
builder.Services.AddSingleton<ITokenIssuer>(sp =>
{
    var cfg = sp.GetRequiredService<Config>();
    return new AurixTokenIssuer(new AurixClient(new AurixClientOptions
    {
        BaseUrl = cfg.AurixUrl,
        Credentials = new Credentials { ApiKey = cfg.ApiKey },
    }));
});

var app = builder.Build();
var config = app.Services.GetRequiredService<Config>();
var log = app.Services.GetRequiredService<ILoggerFactory>().CreateLogger("token_server");

app.MapGet("/healthz", () => Results.Ok(new { ok = true }));

if (config.AllowDevLogin)
{
    app.MapPost("/dev/login", (DevLoginRequest body) =>
        string.IsNullOrEmpty(body.PlayerId) || string.IsNullOrEmpty(body.DisplayName)
            ? Results.BadRequest(new ErrorBody("player_id and display_name required"))
            : Results.Ok(new { session = GameSession.MintDev(config.SessionSecret, body.PlayerId, body.DisplayName, TimeSpan.FromHours(1)) }));
}

app.MapPost("/voice/token", async (HttpRequest req, VoiceTokenRequest? body, ITokenIssuer aurix, CancellationToken ct) =>
{
    var player = GameSession.Authenticate(config.SessionSecret, req.Headers.Authorization);
    if (player is null)
        return Results.Json(new ErrorBody("not logged in"), statusCode: StatusCodes.Status401Unauthorized);
    if (!VoiceTokens.PlayerMayJoin(player, body?.MatchId))
        return Results.Json(new ErrorBody("not allowed to join this match"), statusCode: StatusCodes.Status403Forbidden);
    try
    {
        return Results.Ok(await VoiceTokens.IssueAsync(aurix, player, body!.MatchId!, config.Region, ct));
    }
    catch (AurixException e)
    {
        // Aurix' message may describe our request; log it server-side, never forward it verbatim.
        log.LogError("aurix {Status} {Code} (request {RequestId})", e.Status, e.Code, e.RequestId ?? "-");
        return Results.Json(new ErrorBody("voice service unavailable"),
            statusCode: e.IsRateLimited ? StatusCodes.Status503ServiceUnavailable : StatusCodes.Status502BadGateway);
    }
    catch (AurixNetworkException e)
    {
        log.LogError("aurix unreachable: {Message}", e.Message);
        return Results.Json(new ErrorBody("voice service unavailable"), statusCode: StatusCodes.Status503ServiceUnavailable);
    }
});

log.LogInformation("token server -> {AurixUrl}{Dev}", config.AurixUrl, config.AllowDevLogin ? " (dev login enabled)" : "");
app.Run();

sealed record DevLoginRequest(
    [property: JsonPropertyName("player_id")] string? PlayerId,
    [property: JsonPropertyName("display_name")] string? DisplayName);

sealed record VoiceTokenRequest([property: JsonPropertyName("match_id")] string? MatchId);

sealed record ErrorBody([property: JsonPropertyName("error")] string Error);

/// <summary>Exposed for <c>WebApplicationFactory</c> in the tests.</summary>
public partial class Program;
