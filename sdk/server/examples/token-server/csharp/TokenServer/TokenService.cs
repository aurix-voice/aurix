using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;
using Aurix.Server;

namespace Aurix.Examples.TokenServer;

/// <summary>Configuration read from the backend environment; see <see cref="Load"/>.</summary>
public sealed record Config(string AurixUrl, string ApiKey, string SessionSecret, string? Region, bool AllowDevLogin)
{
    public static Config Load(Func<string, string?> getenv)
    {
        var keyFile = getenv("AURIX_API_KEY_FILE");
        var apiKey = string.IsNullOrEmpty(keyFile) ? getenv("AURIX_API_KEY")?.Trim() : File.ReadAllText(keyFile).Trim();
        if (string.IsNullOrEmpty(apiKey))
            throw new InvalidOperationException("set AURIX_API_KEY or AURIX_API_KEY_FILE (backend environment only)");
        var secret = getenv("GAME_SESSION_SECRET") ?? "";
        if (secret.Length < 32)
            throw new InvalidOperationException("GAME_SESSION_SECRET must be >= 32 characters");
        var region = getenv("AURIX_REGION");
        if (string.IsNullOrEmpty(region)) region = null;
        else if (!Aurix.Server.Region.All.Contains(region))
            throw new InvalidOperationException($"AURIX_REGION must be one of {string.Join(", ", Aurix.Server.Region.All)}");
        return new Config(
            AurixUrl: string.IsNullOrEmpty(getenv("AURIX_URL")) ? "http://localhost:8080" : getenv("AURIX_URL")!,
            ApiKey: apiKey,
            SessionSecret: secret,
            Region: region,
            AllowDevLogin: getenv("ALLOW_DEV_LOGIN") == "1");
    }
}

/// <summary>Identity established by the game's own authentication.</summary>
public sealed record Player(string Id, string DisplayName);

/// <summary>
/// Stand-in for your real login. Replace <see cref="Authenticate"/> with your own session/JWT
/// validation; what matters is that the player id comes from <em>your</em> auth, not the request body.
/// </summary>
public static class GameSession
{
    private static readonly Regex Bearer = new(@"^Bearer\s+([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)$", RegexOptions.Compiled);

    private sealed record Claims(
        [property: JsonPropertyName("pid")] string? Pid,
        [property: JsonPropertyName("name")] string? Name,
        [property: JsonPropertyName("exp")] long Exp);

    private static string B64(byte[] b) => Convert.ToBase64String(b).TrimEnd('=').Replace('+', '-').Replace('/', '_');

    private static byte[] UnB64(string s)
    {
        var t = s.Replace('-', '+').Replace('_', '/');
        return Convert.FromBase64String(t.PadRight(t.Length + (4 - t.Length % 4) % 4, '='));
    }

    private static byte[] Sign(string secret, string payload) =>
        HMACSHA256.HashData(Encoding.UTF8.GetBytes(secret), Encoding.UTF8.GetBytes(payload));

    public static string MintDev(string secret, string playerId, string displayName, TimeSpan ttl)
    {
        var exp = DateTimeOffset.UtcNow.Add(ttl).ToUnixTimeSeconds();
        var payload = B64(JsonSerializer.SerializeToUtf8Bytes(new Claims(playerId, displayName, exp)));
        return payload + "." + B64(Sign(secret, payload));
    }

    public static Player? Authenticate(string secret, string? authorization)
    {
        var m = Bearer.Match(authorization ?? "");
        if (!m.Success) return null;
        byte[] given;
        try { given = UnB64(m.Groups[2].Value); }
        catch (FormatException) { return null; }
        if (!CryptographicOperations.FixedTimeEquals(given, Sign(secret, m.Groups[1].Value))) return null;
        Claims? claims;
        try { claims = JsonSerializer.Deserialize<Claims>(UnB64(m.Groups[1].Value)); }
        catch (Exception e) when (e is JsonException or FormatException) { return null; }
        if (claims is null || string.IsNullOrEmpty(claims.Pid) || string.IsNullOrEmpty(claims.Name)) return null;
        if (claims.Exp <= DateTimeOffset.UtcNow.ToUnixTimeSeconds()) return null;
        return new Player(claims.Pid, claims.Name);
    }
}

/// <summary>The part of <see cref="AurixClient"/> the token server needs (an interface so tests can stub it).</summary>
public interface ITokenIssuer
{
    Task<TokenResponse> IssueTokenAsync(GenerateTokenRequest body, RequestOptions? options = null, CancellationToken cancellationToken = default);
}

internal sealed class AurixTokenIssuer(AurixClient client) : ITokenIssuer
{
    public Task<TokenResponse> IssueTokenAsync(GenerateTokenRequest body, RequestOptions? options = null, CancellationToken cancellationToken = default) =>
        client.IssueTokenAsync(body, options, cancellationToken);
}

/// <summary>
/// Allow-listed response handed to the game client. New Aurix fields must be opted in here —
/// nothing from upstream is forwarded implicitly.
/// </summary>
public sealed record VoiceToken(
    [property: JsonPropertyName("token")] string Token,
    [property: JsonPropertyName("user_id")] string UserId,
    [property: JsonPropertyName("expires_at")] string ExpiresAt,
    [property: JsonPropertyName("endpoint")] VoiceEndpoint? Endpoint);

public sealed record VoiceEndpoint(
    [property: JsonPropertyName("ws_url")] string WsUrl,
    [property: JsonPropertyName("region")] string Region);

public static class VoiceTokens
{
    private static readonly Regex MatchId = new("^[A-Za-z0-9_-]{1,64}$", RegexOptions.Compiled);

    /// <summary>Game-side authorisation hook: may this player join this match's voice? (stub: any well-formed match)</summary>
    public static bool PlayerMayJoin(Player player, string? matchId) => matchId is not null && MatchId.IsMatch(matchId);

    /// <summary>Calls Aurix with the backend API key for an authenticated player.</summary>
    public static async Task<VoiceToken> IssueAsync(ITokenIssuer aurix, Player player, string matchId, string? region, CancellationToken ct)
    {
        var res = await aurix.IssueTokenAsync(new GenerateTokenRequest
        {
            ExternalId = player.Id,
            DisplayName = player.DisplayName,
            Channels = new List<ChannelGrant>
            {
                new()
                {
                    AdHoc = new AdHocChannel { Name = $"match-{matchId}", ChannelType = ChannelType.Team },
                    Join = true,
                    Speak = true,
                    Receive = true,
                },
            },
            Region = region,
        }, cancellationToken: ct).ConfigureAwait(false);

        return new VoiceToken(
            res.Token,
            res.UserId,
            res.ExpiresAt,
            res.Endpoint is null ? null : new VoiceEndpoint(res.Endpoint.WsUrl, res.Endpoint.Region));
    }
}
