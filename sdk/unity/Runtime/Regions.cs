using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Linq;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;

namespace Aurix
{
    /// <summary>One advertised region (<c>GET /v1/me/regions</c>, <c>endpoint</c> of <c>POST /v1/tokens</c>).</summary>
    public sealed class RegionEndpoint
    {
        public string Region;
        public Guid NodeId;
        /// <summary>Direct <c>wss://</c> URL of the least-loaded node in the region; pass it as the client's control URL.</summary>
        public string WsUrl;
        /// <summary><c>/health</c> of the same node for RTT probing; <c>null</c> when the node has no public API URL.</summary>
        public string ProbeUrl;
        public double? Latitude;
        public double? Longitude;
        /// <summary>Great-circle distance from the location hint, when one was given.</summary>
        public double? DistanceKm;
        /// <summary>Nodes with capacity in the region.</summary>
        public int Nodes;
        /// <summary>Load of the advertised node, <c>0..1</c>.</summary>
        public float LoadFactor;
        /// <summary>Best probe sample in milliseconds; <c>null</c> when not probed or unreachable.</summary>
        public double? RttMs;
        /// <summary><c>true</c> when a probe was attempted and every request failed.</summary>
        public bool ProbeFailed;

        public static RegionEndpoint FromJson(Dictionary<string, object> o)
        {
            if (o == null) throw new FormatException("region endpoint is not an object");
            var region = MiniJson.GetString(o, "region");
            var ws = MiniJson.GetString(o, "ws_url");
            var nodeId = MiniJson.GetGuid(o, "node_id");
            if (region == null || ws == null || nodeId == null)
                throw new FormatException("region endpoint is missing region/node_id/ws_url");
            var loc = MiniJson.AsObject(o.TryGetValue("location", out var l) ? l : null);
            return new RegionEndpoint
            {
                Region = region,
                NodeId = nodeId.Value,
                WsUrl = ws,
                ProbeUrl = MiniJson.GetString(o, "probe_url"),
                Latitude = loc != null && loc.TryGetValue("latitude", out var la) && la is double lat ? lat : (double?)null,
                Longitude = loc != null && loc.TryGetValue("longitude", out var lo) && lo is double lon ? lon : (double?)null,
                DistanceKm = o.TryGetValue("distance_km", out var d) && d is double dk ? dk : (double?)null,
                Nodes = (int)MiniJson.GetNumber(o, "nodes"),
                LoadFactor = (float)MiniJson.GetNumber(o, "load_factor"),
            };
        }
    }

    public sealed class RegionDiscoveryOptions
    {
        /// <summary>Region the game prefers (party leader, matchmaking); ranks first when reachable.</summary>
        public string PreferredRegion;
        /// <summary>Approximate player coordinates (WGS-84) for the server's distance ordering.</summary>
        public double? Latitude;
        public double? Longitude;
        /// <summary>Measure RTT to every region's <see cref="RegionEndpoint.ProbeUrl"/>.</summary>
        public bool Probe = true;
        /// <summary>Timed requests per region after one discarded warm-up.</summary>
        public int ProbeSamples = 3;
        public TimeSpan ProbeTimeout = TimeSpan.FromSeconds(2);
        /// <summary>RTT differences below this are ties broken by the server order (distance, load).</summary>
        public double RttToleranceMs = 15;
    }

    /// <summary>
    /// Region discovery: fetch the regions with capacity, measure RTT to each from the player's network
    /// and rank them. Geography is only a proxy for latency, so the measured RTT overrides the
    /// server's distance ordering; ties keep the server order.
    /// </summary>
    public static class RegionDiscovery
    {
        /// <summary>Ranked best first (<c>[0]</c> is the recommendation); empty when no node is advertised.</summary>
        public static async Task<List<RegionEndpoint>> DiscoverAsync(
            HttpClient http, string apiUrl, string token, RegionDiscoveryOptions options = null, CancellationToken ct = default)
        {
            if (http == null) throw new ArgumentNullException(nameof(http));
            if (string.IsNullOrEmpty(apiUrl)) throw new ArgumentException("apiUrl is required", nameof(apiUrl));
            if (string.IsNullOrEmpty(token)) throw new ArgumentException("token is required", nameof(token));
            options ??= new RegionDiscoveryOptions();

            var url = apiUrl.TrimEnd('/') + "/v1/me/regions" + BuildQuery(options);
            using var req = new HttpRequestMessage(HttpMethod.Get, url);
            req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
            using var res = await http.SendAsync(req, ct).ConfigureAwait(false);
            if (!res.IsSuccessStatusCode)
                throw new HttpRequestException($"region discovery failed: HTTP {(int)res.StatusCode}");
            var regions = ParseResponse(await res.Content.ReadAsStringAsync().ConfigureAwait(false));

            if (options.Probe)
            {
                var probes = regions
                    .Where(r => r.ProbeUrl != null)
                    .Select(async r =>
                    {
                        r.RttMs = await ProbeRttAsync(http, r.ProbeUrl, options.ProbeSamples, options.ProbeTimeout, ct).ConfigureAwait(false);
                        r.ProbeFailed = r.RttMs == null;
                    });
                await Task.WhenAll(probes).ConfigureAwait(false);
            }
            return Rank(regions, options.PreferredRegion, options.RttToleranceMs);
        }

        /// <summary>
        /// Minimum round-trip time over <paramref name="samples"/> GETs (one extra warm-up request pays for
        /// the TLS handshake and is discarded). <c>null</c> when every request failed or timed out.
        /// </summary>
        public static async Task<double?> ProbeRttAsync(HttpClient http, string url, int samples, TimeSpan timeout, CancellationToken ct = default)
        {
            samples = Math.Max(1, samples);
            double? best = null;
            for (int i = 0; i <= samples; i++)
            {
                using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
                cts.CancelAfter(timeout);
                var sw = Stopwatch.StartNew();
                try
                {
                    using var req = new HttpRequestMessage(HttpMethod.Get, url);
                    req.Headers.CacheControl = new CacheControlHeaderValue { NoStore = true };
                    using var res = await http.SendAsync(req, HttpCompletionOption.ResponseContentRead, cts.Token).ConfigureAwait(false);
                    if (!res.IsSuccessStatusCode) return best;
                    double elapsed = sw.Elapsed.TotalMilliseconds;
                    if (i > 0 && (best == null || elapsed < best.Value)) best = elapsed;
                }
                catch (OperationCanceledException) when (ct.IsCancellationRequested)
                {
                    throw;
                }
                catch (Exception)
                {
                    // A failed warm-up means the node is unreachable; a failed sample just does not count.
                    if (i == 0) return null;
                }
            }
            return best;
        }

        /// <summary>
        /// Stable re-ranking of the server's list: the preferred region first (unless its probe failed),
        /// then measured regions in ascending <paramref name="toleranceMs"/> buckets, then regions that
        /// were not probed, and last regions whose probe failed. Within a bucket the server order
        /// (distance, load) is kept.
        /// </summary>
        public static List<RegionEndpoint> Rank(IEnumerable<RegionEndpoint> regions, string preferred, double toleranceMs = 15)
        {
            double tolerance = Math.Max(1, toleranceMs);
            long Key(RegionEndpoint r)
            {
                bool measured = r.RttMs.HasValue && !double.IsNaN(r.RttMs.Value) && !double.IsInfinity(r.RttMs.Value);
                bool isPreferred = preferred != null && r.Region == preferred;
                if (isPreferred && (measured || !r.ProbeFailed)) return -1;
                if (!measured) return r.ProbeFailed ? long.MaxValue : long.MaxValue - 1;
                return (long)Math.Floor(r.RttMs.Value / tolerance);
            }
            return regions.Select((r, index) => (r, index, k: Key(r)))
                .OrderBy(x => x.k).ThenBy(x => x.index)
                .Select(x => x.r)
                .ToList();
        }

        public static List<RegionEndpoint> ParseResponse(string json)
        {
            var root = MiniJson.AsObject(MiniJson.Parse(json));
            var arr = MiniJson.AsArray(root != null && root.TryGetValue("regions", out var v) ? v : null);
            if (arr == null) throw new FormatException("region discovery: malformed response");
            return arr.Select(e => RegionEndpoint.FromJson(MiniJson.AsObject(e))).ToList();
        }

        private static string BuildQuery(RegionDiscoveryOptions o)
        {
            var parts = new List<string>();
            if (!string.IsNullOrEmpty(o.PreferredRegion)) parts.Add("region=" + Uri.EscapeDataString(o.PreferredRegion));
            if (o.Latitude.HasValue && o.Longitude.HasValue)
            {
                parts.Add("latitude=" + o.Latitude.Value.ToString("R", CultureInfo.InvariantCulture));
                parts.Add("longitude=" + o.Longitude.Value.ToString("R", CultureInfo.InvariantCulture));
            }
            return parts.Count == 0 ? string.Empty : "?" + string.Join("&", parts);
        }
    }
}
