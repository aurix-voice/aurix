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
    /// WebSocket control channel. Authentication uses the <c>bearer.&lt;jwt&gt;</c> subprotocol (the
    /// same mechanism as the Web SDK) so it works on every .NET/Unity backend, including those
    /// that do not let callers set the Authorization header on the upgrade request.
    /// Received messages are queued and delivered by <see cref="Drain"/> on the caller's thread.
    /// </summary>
    public sealed class ControlChannel : IDisposable
    {
        public const string AurixSubprotocol = "aurix";
        public const string BearerSubprotocolPrefix = "bearer.";

        private readonly ConcurrentQueue<ControlMessage> _inbox = new ConcurrentQueue<ControlMessage>();
        private readonly SemaphoreSlim _sendLock = new SemaphoreSlim(1, 1);
        private ClientWebSocket _ws;
        private CancellationTokenSource _cts;
        private Task _readLoop;

        /// <summary>Raised from the read loop thread when the socket closes or fails.</summary>
        public event Action<string> Closed;
        /// <summary>Raised on the read loop thread for every message, before it is queued for <see cref="Drain"/>.</summary>
        public event Action<ControlMessage> Received;

        public bool IsOpen => _ws != null && _ws.State == WebSocketState.Open;

        public async Task ConnectAsync(Uri wsUrl, string jwt, CancellationToken ct)
        {
            if (_ws != null) throw new InvalidOperationException("already connected");
            var ws = new ClientWebSocket();
            ws.Options.AddSubProtocol(AurixSubprotocol);
            ws.Options.AddSubProtocol(BearerSubprotocolPrefix + jwt);
            ws.Options.KeepAliveInterval = TimeSpan.FromSeconds(20);
            await ws.ConnectAsync(wsUrl, ct).ConfigureAwait(false);
            _ws = ws;
            _cts = new CancellationTokenSource();
            _readLoop = Task.Run(() => ReadLoop(_cts.Token));
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
