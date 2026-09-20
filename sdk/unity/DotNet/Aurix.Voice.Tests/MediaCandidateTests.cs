using System.Net;
using System.Net.Sockets;
using System.Threading.Tasks;
using Aurix.Transport;
using Xunit;

namespace Aurix.Voice.Tests
{
    /// <summary>IPv4 / IPv6 media endpoint candidates from <c>SessionInitAck</c>.</summary>
    public class MediaCandidateTests
    {
        [Fact]
        public async Task DualStackNodeYieldsIpv4ThenIpv6Candidates()
        {
            var c = await MediaTransport.ResolveCandidatesAsync("203.0.113.7:9000", new[] { "203.0.113.7:9000", "[2001:db8::7]:9000" });
            Assert.Equal(2, c.Count);
            Assert.Equal(new IPEndPoint(IPAddress.Parse("203.0.113.7"), 9000), c[0]);
            Assert.Equal(AddressFamily.InterNetworkV6, c[1].AddressFamily);
            Assert.Equal(new IPEndPoint(IPAddress.Parse("2001:db8::7"), 9000), c[1]);
        }

        [Fact]
        public async Task OlderNodesWithoutMediaAddrsFallBackToMediaAddr()
        {
            var c = await MediaTransport.ResolveCandidatesAsync("[::1]:9001", null);
            Assert.Single(c);
            Assert.Equal(new IPEndPoint(IPAddress.IPv6Loopback, 9001), c[0]);
            c = await MediaTransport.ResolveCandidatesAsync("127.0.0.1:9001", new string[0]);
            Assert.Single(c);
            Assert.Equal(new IPEndPoint(IPAddress.Loopback, 9001), c[0]);
        }

        [Fact]
        public async Task MalformedEntriesAreSkippedAndDuplicatesCollapse()
        {
            var c = await MediaTransport.ResolveCandidatesAsync("127.0.0.1:1", new[] { "nonsense", "127.0.0.1:1", "127.0.0.1:1" });
            Assert.Single(c);
            await Assert.ThrowsAnyAsync<System.Exception>(() => MediaTransport.ResolveCandidatesAsync("nonsense", new[] { "nonsense" }));
        }

        [Fact]
        public async Task ResolveAsyncKeepsPreferringIpv4()
        {
            var ep = await MediaTransport.ResolveAsync("[::1]:9001");
            Assert.Equal(new IPEndPoint(IPAddress.IPv6Loopback, 9001), ep);
            ep = await MediaTransport.ResolveAsync("localhost:9002");
            Assert.Equal(9002, ep.Port);
        }
    }
}
