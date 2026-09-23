using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Net.Security;
using System.Net.Sockets;
using System.Security.Authentication;
using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;

namespace Aurix.Transport
{
    /// <summary>What the node advertised in <c>SessionInitAck.tls_tunnel</c>: where its TLS media tunnel listens and how to pin it.</summary>
    public sealed class TlsTunnelInfo
    {
        /// <summary><c>host:port</c> endpoints of the tunnel, in the node's preferred order (usually port 443).</summary>
        public IReadOnlyList<string> Addrs = Array.Empty<string>();
        /// <summary>Lowercase hex SHA-256 of the node's DER certificate; the only thing the client trusts.</summary>
        public string CertSha256;
        /// <summary>Name sent as SNI (the certificate's subject alternative name).</summary>
        public string ServerName;
    }

    /// <summary>
    /// The node's dedicated TLS media tunnel as an <see cref="IMediaTunnel"/>: a TLS 1.3 connection
    /// (<see cref="SslStream"/>, ALPN <c>aurix-tunnel/1</c>) to a separate TCP port — normally 443 —
    /// pinned to the certificate fingerprint the node advertised over the authenticated control
    /// channel, not to any CA. Inside, every sealed AURX packet is one <c>u16 big-endian length | packet</c>
    /// frame; the AURX layer keeps authenticating, encrypting and replay-checking exactly as on UDP.
    /// Frames are bounded (no empty frames, nothing above <see cref="AurxPacket.MaxPacketSize"/>);
    /// a violation in either direction closes the connection.
    /// </summary>
    public sealed class TlsMediaTunnel : IMediaTunnel, IDisposable
    {
        public const string Alpn = "aurix-tunnel/1";
        /// <summary>Outbound frames waiting for the socket; beyond this they are dropped like lost datagrams.</summary>
        public const int SendQueueLength = 64;

        private readonly TcpClient _tcp;
        private readonly SslStream _ssl;
        private readonly BlockingCollection<byte[]> _outbox = new BlockingCollection<byte[]>(SendQueueLength);
        private readonly CancellationTokenSource _cts = new CancellationTokenSource();
        private readonly Task _reader;
        private readonly Task _writer;
        private long _packetsSent, _packetsDropped, _framesReceived;
        private int _closed;
        private string _closeReason;

        public event Action<byte[]> MediaReceived;
        /// <summary>Raised once, from a transport thread, when the connection is gone (with the reason).</summary>
        public event Action<string> Closed;

        public IPEndPoint RemoteEndPoint { get; }
        public IPEndPoint LocalEndPoint { get; }
        /// <summary>Negotiated TLS version (informational).</summary>
        public SslProtocols Protocol => _ssl.SslProtocol;
        public bool IsClosed => Volatile.Read(ref _closed) != 0;
        public string CloseReason => Volatile.Read(ref _closeReason);
        public long PacketsSent => Interlocked.Read(ref _packetsSent);
        public long PacketsDropped => Interlocked.Read(ref _packetsDropped);
        public long FramesReceived => Interlocked.Read(ref _framesReceived);

        private TlsMediaTunnel(TcpClient tcp, SslStream ssl)
        {
            _tcp = tcp;
            _ssl = ssl;
            RemoteEndPoint = tcp.Client.RemoteEndPoint as IPEndPoint;
            LocalEndPoint = tcp.Client.LocalEndPoint as IPEndPoint;
            _reader = Task.Run(() => ReadLoop(_cts.Token));
            _writer = Task.Run(() => WriteLoop(_cts.Token));
        }

        /// <summary>
        /// Connect to <paramref name="endpoint"/>, complete the pinned TLS handshake (SNI <paramref name="serverName"/>,
        /// certificate SHA-256 <paramref name="certSha256"/>, ALPN <see cref="Alpn"/>) and start the frame loops.
        /// Fails when the peer presents another certificate or does not speak the tunnel protocol.
        /// </summary>
        public static async Task<TlsMediaTunnel> ConnectAsync(IPEndPoint endpoint, string serverName, string certSha256, TimeSpan timeout, CancellationToken ct)
        {
            if (endpoint == null) throw new ArgumentNullException(nameof(endpoint));
            var pin = NormalizePin(certSha256);
            var tcp = new TcpClient(endpoint.AddressFamily) { NoDelay = true };
            SslStream ssl = null;
            try
            {
                using (var timer = CancellationTokenSource.CreateLinkedTokenSource(ct))
                {
                    timer.CancelAfter(timeout);
                    var connect = tcp.ConnectAsync(endpoint.Address, endpoint.Port);
                    var done = await Task.WhenAny(connect, Task.Delay(Timeout.Infinite, timer.Token)).ConfigureAwait(false);
                    if (done != connect)
                    {
                        ct.ThrowIfCancellationRequested();
                        throw new TimeoutException($"TLS tunnel connect to {endpoint} timed out");
                    }
                    await connect.ConfigureAwait(false);
                    ssl = new SslStream(tcp.GetStream(), false, (sender, cert, chain, errors) => PinMatches(cert, pin));
                    var options = new SslClientAuthenticationOptions
                    {
                        TargetHost = string.IsNullOrEmpty(serverName) ? endpoint.Address.ToString() : serverName,
                        ApplicationProtocols = new List<SslApplicationProtocol> { new SslApplicationProtocol(Alpn) },
                        EnabledSslProtocols = SslProtocols.None,
                        CertificateRevocationCheckMode = X509RevocationMode.NoCheck,
                    };
                    var handshake = ssl.AuthenticateAsClientAsync(options, timer.Token);
                    done = await Task.WhenAny(handshake, Task.Delay(Timeout.Infinite, timer.Token)).ConfigureAwait(false);
                    if (done != handshake)
                    {
                        ct.ThrowIfCancellationRequested();
                        throw new TimeoutException($"TLS tunnel handshake with {endpoint} timed out");
                    }
                    await handshake.ConfigureAwait(false);
                }
                var negotiated = ssl.NegotiatedApplicationProtocol.Protocol;
                if (negotiated.Length == 0 || Encoding.ASCII.GetString(negotiated.ToArray()) != Alpn)
                    throw new AuthenticationException("the TLS tunnel peer did not negotiate " + Alpn);
                return new TlsMediaTunnel(tcp, ssl);
            }
            catch
            {
                ssl?.Dispose();
                tcp.Dispose();
                throw;
            }
        }

        /// <summary>Lowercase hex SHA-256 of a DER certificate, the form the node advertises.</summary>
        public static string Fingerprint(byte[] der)
        {
            using (var sha = SHA256.Create())
            {
                var hash = sha.ComputeHash(der);
                var sb = new StringBuilder(hash.Length * 2);
                foreach (var b in hash) sb.Append(b.ToString("x2"));
                return sb.ToString();
            }
        }

        /// <summary>True when <paramref name="cert"/> is exactly the pinned certificate (chain and CA are irrelevant).</summary>
        public static bool PinMatches(X509Certificate cert, string certSha256)
        {
            if (cert == null) return false;
            var pin = NormalizePin(certSha256);
            return Fingerprint(cert.GetRawCertData()) == pin;
        }

        private static string NormalizePin(string certSha256)
        {
            if (string.IsNullOrEmpty(certSha256)) throw new ArgumentException("certificate pin required", nameof(certSha256));
            var pin = certSha256.Replace(":", "").Trim().ToLowerInvariant();
            if (pin.Length != 64) throw new ArgumentException("certificate pin must be a hex SHA-256", nameof(certSha256));
            foreach (var c in pin)
                if (!Uri.IsHexDigit(c)) throw new ArgumentException("certificate pin must be a hex SHA-256", nameof(certSha256));
            return pin;
        }

        /// <summary>Encode one packet as a tunnel frame; null when it cannot travel (empty or oversized).</summary>
        public static byte[] EncodeFrame(byte[] packet)
        {
            if (packet == null || packet.Length == 0 || packet.Length > AurxPacket.MaxPacketSize) return null;
            var frame = new byte[2 + packet.Length];
            frame[0] = (byte)(packet.Length >> 8);
            frame[1] = (byte)packet.Length;
            Buffer.BlockCopy(packet, 0, frame, 2, packet.Length);
            return frame;
        }

        public bool TrySendMedia(byte[] wire)
        {
            if (IsClosed) return false;
            var frame = EncodeFrame(wire);
            if (frame == null) { Interlocked.Increment(ref _packetsDropped); return false; }
            bool queued;
            try { queued = _outbox.TryAdd(frame); }
            catch (ObjectDisposedException) { queued = false; }
            catch (InvalidOperationException) { queued = false; }
            if (queued) Interlocked.Increment(ref _packetsSent);
            else Interlocked.Increment(ref _packetsDropped);
            return queued;
        }

        private async Task WriteLoop(CancellationToken ct)
        {
            try
            {
                while (!ct.IsCancellationRequested)
                {
                    byte[] frame;
                    try { frame = _outbox.Take(ct); }
                    catch (InvalidOperationException) { return; }
                    await _ssl.WriteAsync(frame, 0, frame.Length, ct).ConfigureAwait(false);
                    // Coalesce what queued up meanwhile before flushing.
                    while (_outbox.TryTake(out var next))
                        await _ssl.WriteAsync(next, 0, next.Length, ct).ConfigureAwait(false);
                    await _ssl.FlushAsync(ct).ConfigureAwait(false);
                }
            }
            catch (OperationCanceledException) { }
            catch (Exception e) { Close("write failed: " + e.Message); }
        }

        private async Task ReadLoop(CancellationToken ct)
        {
            var header = new byte[2];
            var body = new byte[AurxPacket.MaxPacketSize];
            try
            {
                while (!ct.IsCancellationRequested)
                {
                    if (!await ReadExactAsync(header, 2, ct).ConfigureAwait(false)) { Close("closed by peer"); return; }
                    int len = (header[0] << 8) | header[1];
                    if (len == 0) { Close("empty frame from peer"); return; }
                    if (len > AurxPacket.MaxPacketSize) { Close($"oversized frame from peer ({len} bytes)"); return; }
                    if (!await ReadExactAsync(body, len, ct).ConfigureAwait(false)) { Close("EOF inside a frame"); return; }
                    var packet = new byte[len];
                    Buffer.BlockCopy(body, 0, packet, 0, len);
                    Interlocked.Increment(ref _framesReceived);
                    MediaReceived?.Invoke(packet);
                }
            }
            catch (OperationCanceledException) { }
            catch (Exception e) { Close("read failed: " + e.Message); }
        }

        private async Task<bool> ReadExactAsync(byte[] buf, int len, CancellationToken ct)
        {
            int got = 0;
            while (got < len)
            {
                int n = await _ssl.ReadAsync(buf, got, len - got, ct).ConfigureAwait(false);
                if (n <= 0)
                {
                    if (got == 0) return false;
                    throw new IOException("EOF inside a frame");
                }
                got += n;
            }
            return true;
        }

        private void Close(string reason)
        {
            if (Interlocked.CompareExchange(ref _closeReason, reason, null) != null) return;
            Volatile.Write(ref _closed, 1);
            _cts.Cancel();
            try { _outbox.CompleteAdding(); } catch (ObjectDisposedException) { }
            try { _ssl.Dispose(); } catch (Exception) { }
            try { _tcp.Dispose(); } catch (Exception) { }
            Closed?.Invoke(reason);
        }

        public void Dispose()
        {
            Close("disposed");
            _cts.Dispose();
            _outbox.Dispose();
        }
    }
}
