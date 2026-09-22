using System;
using System.Collections.Concurrent;
using System.IO;
using System.Net.WebSockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aurix.Protocol;

namespace Aurix.Transport
{
    /// <summary>
    /// A reliable, ordered byte-frame link that can carry sealed AURX packets when UDP cannot:
    /// one packet per frame, in both directions. <see cref="ControlChannel"/> implements it over
    /// the control WebSocket (binary frames); tests can plug in a fake.
    /// </summary>
    public interface IMediaTunnel
    {
        /// <summary>
        /// Queue one sealed packet for sending without blocking. Returns false when the link is
        /// closed or its bounded queue is full (the packet is dropped, like a lost datagram).
        /// </summary>
        bool TrySendMedia(byte[] wire);
        /// <summary>Raised on the link's read thread for every inbound media frame.</summary>
        event Action<byte[]> MediaReceived;
    }

    /// <summary>
    /// WebSocket control channel. Authentication uses the <c>bearer.&lt;jwt&gt;</c> subprotocol (the
    /// same mechanism as the Web SDK) so it works on every .NET/Unity backend, including those
    /// that do not let callers set the Authorization header on the upgrade request.
    /// Received messages are queued and delivered by <see cref="Drain"/> on the caller's thread.
    /// Binary frames are AURX media for a tunnelled <see cref="MediaTransport"/> (see
    /// <see cref="IMediaTunnel"/>); they never enter the control inbox.
    /// </summary>
    public sealed class ControlChannel : IDisposable, IMediaTunnel
    {
        public const string AurixSubprotocol = "aurix";
        public const string BearerSubprotocolPrefix = "bearer.";
        public const string ResumeSubprotocolPrefix = "resume.";
        public const string DeviceSubprotocolPrefix = "device.";
        /// <summary>Outbound media frames waiting for the socket; beyond this they are dropped.</summary>
        public const int MediaQueueLength = 64;

        private readonly ConcurrentQueue<ControlMessage> _inbox = new ConcurrentQueue<ControlMessage>();
        private readonly SemaphoreSlim _sendLock = new SemaphoreSlim(1, 1);
        private readonly ConcurrentQueue<byte[]> _mediaOutbox = new ConcurrentQueue<byte[]>();
        private readonly SemaphoreSlim _mediaSignal = new SemaphoreSlim(0);
        private int _mediaQueued;
        private ClientWebSocket _ws;
        private CancellationTokenSource _cts;
        private Task _readLoop;
        private Task _mediaPump;

        /// <summary>Raised from the read loop thread when the socket closes or fails.</summary>
        public event Action<string> Closed;
        /// <summary>Raised on the read loop thread for every message, before it is queued for <see cref="Drain"/>.</summary>
        public event Action<ControlMessage> Received;
        /// <summary>Raised on the read loop thread for every binary (media) frame.</summary>
        public event Action<byte[]> MediaReceived;

        public bool IsOpen => _ws != null && _ws.State == WebSocketState.Open;

        /// <param name="resume">
        /// Optional <c>&lt;session_id&gt;.&lt;resume_token&gt;</c> from a previous <c>SessionInitAck</c>; the
        /// server reattaches that session instead of creating a new one if it is still within its grace period.
        /// </param>
        /// <param name="device">
        /// Optional installation id (<c>[A-Za-z0-9._~-]{1,128}</c>) keying the server's per-device chat delivery cursor.
        /// </param>
        public async Task ConnectAsync(Uri wsUrl, string jwt, CancellationToken ct, string resume = null, string device = null)
        {
            if (_ws != null) throw new InvalidOperationException("already connected");
            var ws = new ClientWebSocket();
            ws.Options.AddSubProtocol(AurixSubprotocol);
            ws.Options.AddSubProtocol(BearerSubprotocolPrefix + jwt);
            if (!string.IsNullOrEmpty(resume)) ws.Options.AddSubProtocol(ResumeSubprotocolPrefix + resume);
            if (!string.IsNullOrEmpty(device)) ws.Options.AddSubProtocol(DeviceSubprotocolPrefix + device);
            ws.Options.KeepAliveInterval = TimeSpan.FromSeconds(20);
            await ws.ConnectAsync(wsUrl, ct).ConfigureAwait(false);
            _ws = ws;
            _cts = new CancellationTokenSource();
            _readLoop = Task.Run(() => ReadLoop(_cts.Token));
            _mediaPump = Task.Run(() => MediaPump(_cts.Token));
        }

        public async Task SendAsync(string json, CancellationToken ct = default)
        {
            var ws = _ws;
            if (ws == null || ws.State != WebSocketState.Open) throw new InvalidOperationException("control channel not open");
            var bytes = Encoding.UTF8.GetBytes(json);
            await _sendLock.WaitAsync(ct).ConfigureAwait(false);
            try
            {
                await ws.SendAsync(new ArraySegment<byte>(bytes), WebSocketMessageType.Text, true, ct).ConfigureAwait(false);
            }
            finally
            {
                _sendLock.Release();
            }
        }

        /// <inheritdoc/>
        public bool TrySendMedia(byte[] wire)
        {
            if (wire == null) throw new ArgumentNullException(nameof(wire));
            var ws = _ws;
            if (ws == null || ws.State != WebSocketState.Open) return false;
            if (Interlocked.Increment(ref _mediaQueued) > MediaQueueLength)
            {
                Interlocked.Decrement(ref _mediaQueued);
                return false;
            }
            _mediaOutbox.Enqueue(wire);
            _mediaSignal.Release();
            return true;
        }

        private async Task MediaPump(CancellationToken ct)
        {
            var ws = _ws;
            try
            {
                while (!ct.IsCancellationRequested)
                {
                    await _mediaSignal.WaitAsync(ct).ConfigureAwait(false);
                    if (!_mediaOutbox.TryDequeue(out var wire)) continue;
                    Interlocked.Decrement(ref _mediaQueued);
                    if (ws.State != WebSocketState.Open) return;
                    await _sendLock.WaitAsync(ct).ConfigureAwait(false);
                    try
                    {
                        await ws.SendAsync(new ArraySegment<byte>(wire), WebSocketMessageType.Binary, true, ct).ConfigureAwait(false);
                    }
                    finally
                    {
                        _sendLock.Release();
                    }
                }
            }
            catch (OperationCanceledException) { }
            catch (ObjectDisposedException) { }
            catch (WebSocketException) { /* the read loop reports the closure */ }
        }

        /// <summary>Dequeue every message received since the last call.</summary>
        public int Drain(Action<ControlMessage> handler)
        {
            int n = 0;
            while (_inbox.TryDequeue(out var m))
            {
                handler(m);
                n++;
            }
            return n;
        }

        /// <summary>Await the next message (used for the connect/join handshakes).</summary>
        public async Task<ControlMessage> NextAsync(TimeSpan timeout, CancellationToken ct)
        {
            var deadline = DateTime.UtcNow + timeout;
            while (DateTime.UtcNow < deadline)
            {
                if (_inbox.TryDequeue(out var m)) return m;
                if (!IsOpen) throw new IOException("control channel closed");
                await Task.Delay(5, ct).ConfigureAwait(false);
            }
            throw new TimeoutException("timed out waiting for control message");
        }

        private async Task ReadLoop(CancellationToken ct)
        {
            var ws = _ws;
            var buffer = new byte[16 * 1024];
            var ms = new MemoryStream();
            string reason = "closed";
            try
            {
                while (!ct.IsCancellationRequested && ws.State == WebSocketState.Open)
                {
                    ms.SetLength(0);
                    WebSocketReceiveResult r;
                    do
                    {
                        r = await ws.ReceiveAsync(new ArraySegment<byte>(buffer), ct).ConfigureAwait(false);
                        if (r.MessageType == WebSocketMessageType.Close)
                        {
                            reason = r.CloseStatusDescription ?? "closed by server";
                            return;
                        }
                        ms.Write(buffer, 0, r.Count);
                    } while (!r.EndOfMessage);

                    if (r.MessageType == WebSocketMessageType.Binary)
                    {
                        var media = MediaReceived;
                        if (media != null && ms.Length > 0 && ms.Length <= AurxPacket.MaxPacketSize)
                        {
                            // A misbehaving media consumer must not take the control plane down with it.
                            try { media(ms.ToArray()); } catch (Exception) { }
                        }
                        continue;
                    }
                    if (r.MessageType != WebSocketMessageType.Text) continue;
                    var text = Encoding.UTF8.GetString(ms.GetBuffer(), 0, (int)ms.Length);
                    try
                    {
                        var msg = ControlMessage.Parse(text);
                        Received?.Invoke(msg);
                        _inbox.Enqueue(msg);
                    }
                    catch (FormatException)
                    {
                        // Unknown/unparseable frame: ignore, the protocol is forward-compatible.
                    }
                }
            }
            catch (OperationCanceledException) { reason = "disconnected"; }
            catch (Exception e) { reason = e.Message; }
            finally
            {
                Closed?.Invoke(reason);
            }
        }

        public async Task CloseAsync(string reason = "client disconnect")
        {
            var ws = _ws;
            if (ws == null) return;
            try
            {
                if (ws.State == WebSocketState.Open)
                {
                    using (var cts = new CancellationTokenSource(TimeSpan.FromSeconds(2)))
                        await ws.CloseAsync(WebSocketCloseStatus.NormalClosure, reason, cts.Token).ConfigureAwait(false);
                }
            }
            catch (Exception) { /* best effort */ }
            Dispose();
        }

        public void Dispose()
        {
            _cts?.Cancel();
            _ws?.Dispose();
            _ws = null;
            _cts?.Dispose();
            _cts = null;
        }
    }
}
