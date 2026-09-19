using System;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Net.Http;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aurix;
using Xunit;

namespace Aurix.Voice.Tests
{
    public class RegionTests
    {
        private static RegionEndpoint Ep(string region, string node, double? rtt = null, bool failed = false, bool probe = true) => new RegionEndpoint
        {
            Region = region,
            NodeId = Guid.NewGuid(),
            WsUrl = $"wss://{node}.example/ws",
            ProbeUrl = probe ? $"https://{node}.example/health" : null,
            RttMs = rtt,
            ProbeFailed = failed,
        };

        private static string[] Names(IEnumerable<RegionEndpoint> l) => l.Select(r => r.WsUrl.Split('.')[0].Substring(6)).ToArray();

        [Fact]
        public void RankPreferredThenRttBucketsThenUnmeasured()
        {
            var regions = new[] { Ep("eu_west", "eu1", 40), Ep("eu_central", "eu2", 30), Ep("us_east", "us1", 120), Ep("africa", "af1", probe: false) };
            Assert.Equal(new[] { "eu1", "eu2", "us1", "af1" }, Names(RegionDiscovery.Rank(regions, null)));
            Assert.Equal(new[] { "eu2", "eu1", "us1", "af1" }, Names(RegionDiscovery.Rank(regions, null, 5)));
            Assert.Equal(new[] { "us1", "eu1", "eu2", "af1" }, Names(RegionDiscovery.Rank(regions, "us_east")));
            Assert.Equal(new[] { "af1", "eu1", "eu2", "us1" }, Names(RegionDiscovery.Rank(regions, "africa")));
            var dead = new[] { Ep("eu_west", "eu1", 40), Ep("us_east", "us1", failed: true), Ep("africa", "af1", probe: false) };
            Assert.Equal(new[] { "eu1", "af1", "us1" }, Names(RegionDiscovery.Rank(dead, "us_east")));
        }

        [Fact]
        public void ParsesTheWireShape()
        {
            var id = Guid.NewGuid();
            var json = "{\"regions\":[{\"region\":\"eu_west\",\"node_id\":\"" + id + "\",\"ws_url\":\"wss://eu1/ws\",\"probe_url\":\"https://eu1/health\"," +
                       "\"location\":{\"latitude\":48.8,\"longitude\":2.3},\"distance_km\":12.5,\"nodes\":2,\"load_factor\":0.25}," +
                       "{\"region\":\"africa\",\"node_id\":\"" + Guid.NewGuid() + "\",\"ws_url\":\"wss://af/ws\",\"probe_url\":null,\"location\":null,\"distance_km\":null,\"nodes\":1,\"load_factor\":0}]," +
                       "\"recommended\":null}";
            var regions = RegionDiscovery.ParseResponse(json);
            Assert.Equal(2, regions.Count);
            Assert.Equal(id, regions[0].NodeId);
            Assert.Equal(48.8, regions[0].Latitude);
            Assert.Equal(12.5, regions[0].DistanceKm);
            Assert.Equal(2, regions[0].Nodes);
            Assert.Equal(0.25f, regions[0].LoadFactor);
            Assert.Null(regions[1].ProbeUrl);
            Assert.Null(regions[1].Latitude);
            Assert.Throws<FormatException>(() => RegionDiscovery.ParseResponse("{}"));
            Assert.Throws<FormatException>(() => RegionDiscovery.ParseResponse("{\"regions\":[{\"region\":\"x\"}]}"));
        }

        private sealed class FakeHandler : HttpMessageHandler
        {
            public readonly List<HttpRequestMessage> Requests = new List<HttpRequestMessage>();
            public Func<HttpRequestMessage, Task<HttpResponseMessage>> Handler;

            protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
            {
                Requests.Add(request);
                return Handler(request);
            }
        }

        private static HttpResponseMessage Json(string body) => new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new StringContent(body, Encoding.UTF8, "application/json"),
        };

        [Fact]
        public async Task ProbeDiscardsWarmupAndReportsUnreachable()
        {
            var handler = new FakeHandler { Handler = _ => Task.FromResult(Json("{}")) };
            using var http = new HttpClient(handler);
            var rtt = await RegionDiscovery.ProbeRttAsync(http, "https://n/health", 3, TimeSpan.FromSeconds(1));
            Assert.Equal(4, handler.Requests.Count);
            Assert.True(rtt.HasValue && rtt.Value >= 0);

            handler.Handler = _ => throw new HttpRequestException("refused");
            Assert.Null(await RegionDiscovery.ProbeRttAsync(http, "https://n/health", 2, TimeSpan.FromSeconds(1)));

            handler.Handler = _ => Task.FromResult(new HttpResponseMessage(HttpStatusCode.ServiceUnavailable));
            Assert.Null(await RegionDiscovery.ProbeRttAsync(http, "https://n/health", 2, TimeSpan.FromSeconds(1)));
        }

        [Fact]
        public async Task DiscoverSendsHintAndTokenAndProbesEveryRegion()
        {
            var eu = Guid.NewGuid();
            var us = Guid.NewGuid();
            var body = "{\"regions\":[" +
                       "{\"region\":\"eu_west\",\"node_id\":\"" + eu + "\",\"ws_url\":\"wss://eu1.example/ws\",\"probe_url\":\"https://eu1.example/health\",\"location\":null,\"distance_km\":null,\"nodes\":1,\"load_factor\":0.1}," +
                       "{\"region\":\"us_east\",\"node_id\":\"" + us + "\",\"ws_url\":\"wss://us1.example/ws\",\"probe_url\":\"https://us1.example/health\",\"location\":null,\"distance_km\":null,\"nodes\":1,\"load_factor\":0.1}" +
                       "],\"recommended\":null}";
            var handler = new FakeHandler();
            handler.Handler = async req =>
            {
                if (req.RequestUri.AbsolutePath == "/v1/me/regions")
                {
                    Assert.Equal("Bearer tok", req.Headers.Authorization.ToString());
                    return Json(body);
                }
                if (req.RequestUri.Host.StartsWith("us1")) await Task.Delay(40);
                return Json("{\"status\":\"healthy\"}");
            };
            using var http = new HttpClient(handler);

            var ranked = await RegionDiscovery.DiscoverAsync(http, "https://api.example/", "tok", new RegionDiscoveryOptions
            {
                PreferredRegion = "us_east",
                Latitude = 1.5,
                Longitude = -2,
                ProbeSamples = 1,
            });
            Assert.Equal("https://api.example/v1/me/regions?region=us_east&latitude=1.5&longitude=-2", handler.Requests[0].RequestUri.ToString());
            Assert.Equal(4, handler.Requests.Count(r => r.RequestUri.AbsolutePath == "/health"));
            Assert.Equal(us, ranked[0].NodeId);
            Assert.True(ranked[0].RttMs >= 35);

            var fast = await RegionDiscovery.DiscoverAsync(http, "https://api.example", "tok", new RegionDiscoveryOptions { ProbeSamples = 1 });
            Assert.Equal(eu, fast[0].NodeId);

            var off = await RegionDiscovery.DiscoverAsync(http, "https://api.example", "tok", new RegionDiscoveryOptions { Probe = false });
            Assert.Null(off[0].RttMs);
            Assert.False(off[0].ProbeFailed);

            handler.Handler = _ => Task.FromResult(new HttpResponseMessage(HttpStatusCode.Unauthorized));
            await Assert.ThrowsAsync<HttpRequestException>(() => RegionDiscovery.DiscoverAsync(http, "https://api.example", "tok"));
        }
    }
}
