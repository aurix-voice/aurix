// The C sample (../c/voice_loop.c) written against the header-only C++ wrapper.
//
//   c++ -std=c++11 voice_loop.cpp -I../../include -L../../../../target/release -laurix_client -lpthread -o voice_loop
//   ./voice_loop ws://127.0.0.1:8081/ws "$SESSION_JWT" "$CHANNEL_ID" [seconds]
//
// With AURIX_REGIONS_JSON set to a `GET /v1/me/regions` body the ranked regions are printed
// first (the discovery itself is the host's HTTP call; see aurix::Regions).
//
// Exit codes: 0 ok, 2 bad arguments, 3 connect failed, 4 join failed.
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <thread>
#include <vector>

#include "aurix_client.hpp"

namespace {

const double kTwoPi = 6.283185307179586;

// Returns true when the connection is over.
bool handle_event(const aurix::Event& ev) {
    switch (ev.type()) {
    case AURIX_EVENT_STATE_CHANGED:
        std::printf("state -> %d\n", static_cast<int>(ev.state()));
        return false;
    case AURIX_EVENT_SESSION_READY: {
        AurixSessionInfo s;
        if (ev.session(s)) {
            std::printf("session %s ssrc=%u resumed=%d\n", aurix::Uuid(s.session_id).str().c_str(), s.ssrc,
                        static_cast<int>(s.resumed));
        }
        return false;
    }
    case AURIX_EVENT_CHANNEL_JOINED: {
        std::vector<AurixParticipant> roster = ev.participants();
        std::printf("joined %s (%zu participants, transcription=%d)\n", ev.channel_id().str().c_str(),
                    roster.size(), static_cast<int>(ev.flag()));
        for (const AurixParticipant& p : roster) {
            std::printf("  - %s \"%s\" ssrc=%u muted=%d\n", aurix::Uuid(p.user_id).str().c_str(), p.display_name,
                        p.ssrc, static_cast<int>(p.muted));
        }
        return false;
    }
    case AURIX_EVENT_PARTICIPANT_SPEAKING:
        std::printf("speaking %s = %d\n", ev.user_id().str().c_str(), static_cast<int>(ev.flag()));
        return false;
    case AURIX_EVENT_CHAT_MESSAGE: {
        AurixChatMessage m;
        if (ev.chat(m)) {
            std::printf("chat <%s> %s\n", m.sender_name, m.text);
        }
        return false;
    }
    case AURIX_EVENT_AUDIO_POLICY_CHANGED: {
        AurixAudioPolicy p;
        if (ev.audio_policy(p)) {
            std::printf("audio policy -> %u..%u bps fec=%d dtx=%d bandwidth=%d complexity=%d\n", p.min_bitrate_bps,
                        p.bitrate_bps, static_cast<int>(p.fec), static_cast<int>(p.dtx),
                        static_cast<int>(p.max_bandwidth), static_cast<int>(p.complexity));
        }
        return false;
    }
    case AURIX_EVENT_MEDIA_PATH_CHANGED:
        std::printf("media path -> %s (%s)\n", ev.media_path() == AURIX_MEDIA_TUNNEL ? "ws-tunnel" : "udp",
                    ev.message().c_str());
        return false;
    case AURIX_EVENT_ENDPOINT_CHANGED:
        std::printf("endpoint -> %s\n", ev.message().c_str());
        return false;
    case AURIX_EVENT_REQUEST_FAILED:
    case AURIX_EVENT_SERVER_ERROR:
    case AURIX_EVENT_REJOIN_FAILED:
        std::printf("error %s: %s\n", ev.code().c_str(), ev.message().c_str());
        return false;
    case AURIX_EVENT_FAILED_TO_RECOVER:
    case AURIX_EVENT_DISCONNECTED:
    case AURIX_EVENT_KICKED:
        std::printf("connection ended: %s\n", ev.message().c_str());
        return true;
    default:
        std::printf("event %s\n", ev.json().c_str());
        return false;
    }
}

}  // namespace

int main(int argc, char** argv) {
    if (argc < 4) {
        std::fprintf(stderr, "usage: %s <ws-url> <token> <channel-uuid> [seconds]\n", argv[0]);
        return 2;
    }
    const unsigned seconds = argc > 4 ? static_cast<unsigned>(std::atoi(argv[4])) : 5;

    if (const char* regions_json = std::getenv("AURIX_REGIONS_JSON")) {
        aurix::Regions regions = aurix::Regions::parse(regions_json);
        if (!regions.valid()) {
            std::fprintf(stderr, "bad regions JSON: %s\n", aurix::last_error().c_str());
            return 2;
        }
        // A real client would GET each probe_url a few times here and call set_rtt().
        regions.rank();
        std::printf("discovery url: %s\n", aurix::Regions::discovery_url("https://voice.example.com", "").c_str());
        for (const AurixRegionEndpoint& r : regions.all()) {
            std::printf("region %s node=%s ws=%s nodes=%u load=%.2f\n", r.region, aurix::Uuid(r.node_id).str().c_str(),
                        r.ws_url, r.nodes, r.load_factor);
        }
    }

    {
        // Bare codec self-test (what a host with its own audio pipeline would use).
        AurixClientConfig defaults;
        aurix_client_config_default(&defaults);
        AurixEncoderSettings s = defaults.encoder;
        s.bitrate_bps = 24000;
        s.max_bandwidth = AURIX_BANDWIDTH_WIDEBAND;
        aurix::OpusEncoder enc(48000, 1, &s);
        aurix::OpusDecoder dec(48000, 1);
        if (!enc.valid() || !dec.valid()) {
            std::fprintf(stderr, "bare codec init failed: %s\n", aurix::last_error().c_str());
            return 2;
        }
        std::vector<float> tone(960);
        for (std::size_t i = 0; i < tone.size(); ++i) {
            tone[i] = 0.5f * std::sin(static_cast<float>(i) * 440.0f * 6.2831853f / 48000.0f);
        }
        std::uint8_t packet[1275];
        std::vector<float> pcm(960);
        const int len = enc.encode(tone.data(), tone.size(), packet, sizeof packet);
        const int n = len > 0 ? dec.decode(packet, static_cast<std::size_t>(len), pcm.data(), pcm.size()) : -1;
        if (len <= 0 || n != 960) {
            std::fprintf(stderr, "bare codec round trip failed (%d/%d): %s\n", len, n, aurix::last_error().c_str());
            return 2;
        }
        std::printf("bare codec: 20 ms frame -> %d bytes\n", len);
    }

    aurix::Uuid channel;
    if (!aurix::Uuid::parse(argv[3], channel)) {
        std::fprintf(stderr, "bad channel id: %s\n", aurix::last_error().c_str());
        return 2;
    }

    aurix::Config cfg(argv[1], argv[2]);
    cfg.raw.vad_gate = false;  // always send our test tone
    cfg.raw.encoder.complexity = 5;
    aurix::Client client = aurix::Client::create(cfg);
    if (!client) {
        std::fprintf(stderr, "create failed: %s\n", aurix::last_error().c_str());
        return 2;
    }
    std::printf("aurix_client %s\n", aurix::version().c_str());

    if (client.connect() != AURIX_OK) {
        std::fprintf(stderr, "connect failed: %s\n", aurix::last_error().c_str());
        return 3;
    }

    // Connecting is asynchronous: wait for SESSION_READY (or a terminal failure) before joining.
    for (;;) {
        aurix::Event ev = client.wait_event(15000);
        if (!ev) {
            std::fprintf(stderr, "connect failed: timed out waiting for the session\n");
            return 3;
        }
        const AurixEventType type = ev.type();
        if (type == AURIX_EVENT_DISCONNECTED || type == AURIX_EVENT_FAILED_TO_RECOVER) {
            std::fprintf(stderr, "connect failed: %s\n", ev.message().c_str());
            return 3;
        }
        handle_event(ev);
        if (type == AURIX_EVENT_SESSION_READY) {
            break;
        }
    }
    std::printf("node %s, %zu failover node(s)\n", client.endpoint().c_str(), client.failover_endpoints().size());

    std::uint64_t join_request = 0;
    if (client.join_channel(channel, nullptr, &join_request) != AURIX_OK) {
        std::fprintf(stderr, "join failed: %s\n", aurix::last_error().c_str());
        return 4;
    }

    std::vector<float> tone(AURIX_FRAME_SAMPLES);
    std::vector<float> out(AURIX_FRAME_SAMPLES * 2);
    double phase = 0.0;
    std::size_t active_frames = 0;
    bool ended = false;

    for (unsigned tick = 0; tick < seconds * 50 && !ended; ++tick) {
        for (std::size_t i = 0; i < tone.size(); ++i) {
            tone[i] = static_cast<float>(0.3 * std::sin(phase));
            phase += kTwoPi * 440.0 / AURIX_SAMPLE_RATE;
        }
        client.push_capture(tone.data(), tone.size(), AURIX_SAMPLE_RATE, 1);
        if (client.mix_output(out.data(), out.size(), 2) > 0) {
            ++active_frames;
        }
        while (aurix::Event ev = client.poll_event()) {
            if (handle_event(ev)) {
                ended = true;
            }
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }

    AurixStats st;
    if (client.stats(st)) {
        std::printf("sent %llu pkts, received %llu pkts (%llu audio frames), rtt %.1f ms, loss %.1f%%, "
                    "mixed %zu active frames\n",
                    static_cast<unsigned long long>(st.packets_sent),
                    static_cast<unsigned long long>(st.packets_received),
                    static_cast<unsigned long long>(st.audio_frames_received), st.rtt_ms, st.loss_percent,
                    active_frames);
    }

    client.leave_channel(channel);
    client.disconnect();
    return 0;
}
