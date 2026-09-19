/*
 * Minimal Aurix native client in C: connect, join a channel, transmit a test tone for a few
 * seconds while mixing everything we hear, print the roster/chat/transcripts as they arrive.
 *
 *   cc voice_loop.c -I../../include -L../../../../target/release -laurix_client -lpthread -lm -o voice_loop
 *   ./voice_loop ws://127.0.0.1:8081/ws "$SESSION_JWT" "$CHANNEL_ID" [seconds]
 *
 * Exit codes: 0 ok, 2 bad arguments, 3 connect failed, 4 join failed.
 *
 * A real integration pushes microphone PCM from its audio callback with
 * aurix_client_push_capture_* and fills its output callback with aurix_client_mix_output_*;
 * this sample fakes both with a 20 ms timer.
 */
#define _POSIX_C_SOURCE 199309L
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "aurix_client.h"

static const double TWO_PI = 6.283185307179586;

static void sleep_ms(unsigned ms) {
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

static void print_uuid(const char *label, struct AurixUuid id) {
    char text[AURIX_UUID_STRING_LEN];
    aurix_uuid_format(&id, text, sizeof text);
    printf("%s%s", label, text);
}

static int handle_event(struct AurixClient *client, struct AurixEvent *ev) {
    int rc = 0;
    switch (aurix_event_type(ev)) {
    case AURIX_EVENT_STATE_CHANGED:
        printf("state -> %d\n", (int)aurix_event_state(ev));
        break;
    case AURIX_EVENT_SESSION_READY: {
        struct AurixSessionInfo s;
        if (aurix_event_session(ev, &s)) {
            print_uuid("session ", s.session_id);
            printf(" ssrc=%u resumed=%d\n", s.ssrc, (int)s.resumed);
        }
        break;
    }
    case AURIX_EVENT_CHANNEL_JOINED: {
        size_t n = aurix_event_participant_count(ev);
        print_uuid("joined ", aurix_event_channel_id(ev));
        printf(" (%zu participants, transcription=%d)\n", n, (int)aurix_event_flag(ev));
        for (size_t i = 0; i < n; i++) {
            struct AurixParticipant p;
            if (aurix_event_participant(ev, i, &p)) {
                print_uuid("  - ", p.user_id);
                printf(" \"%s\" ssrc=%u muted=%d\n", p.display_name, p.ssrc, (int)p.muted);
            }
        }
        break;
    }
    case AURIX_EVENT_PARTICIPANT_JOINED: {
        struct AurixParticipant p;
        if (aurix_event_participant(ev, 0, &p)) printf("participant joined \"%s\"\n", p.display_name);
        break;
    }
    case AURIX_EVENT_PARTICIPANT_LEFT:
        print_uuid("participant left ", aurix_event_user_id(ev));
        printf("\n");
        break;
    case AURIX_EVENT_PARTICIPANT_SPEAKING:
        print_uuid("speaking ", aurix_event_user_id(ev));
        printf(" = %d\n", (int)aurix_event_flag(ev));
        break;
    case AURIX_EVENT_CHAT_MESSAGE: {
        struct AurixChatMessage m;
        if (aurix_event_chat(ev, &m)) printf("chat <%s> %s\n", m.sender_name, m.text);
        break;
    }
    case AURIX_EVENT_TRANSCRIPT: {
        struct AurixTranscript t;
        if (aurix_event_transcript(ev, &t)) printf("transcript: %s\n", t.text);
        break;
    }
    case AURIX_EVENT_BITRATE_CHANGED:
        printf("bitrate -> %llu bps (%s)\n", (unsigned long long)aurix_event_number(ev),
               aurix_event_message(ev));
        break;
    case AURIX_EVENT_MEDIA_PATH_CHANGED:
        printf("media path -> %s (%s)\n",
               aurix_event_media_path(ev) == AURIX_MEDIA_TUNNEL ? "ws-tunnel" : "udp",
               aurix_event_message(ev));
        break;
    case AURIX_EVENT_AUDIO_POLICY_CHANGED: {
        struct AurixAudioPolicy p;
        if (aurix_event_audio_policy(ev, &p)) {
            printf("audio policy -> %u..%u bps fec=%d dtx=%d bandwidth=%d complexity=%d\n",
                   p.min_bitrate_bps, p.bitrate_bps, (int)p.fec, (int)p.dtx, (int)p.max_bandwidth,
                   (int)p.complexity);
        }
        break;
    }
    case AURIX_EVENT_REQUEST_FAILED:
    case AURIX_EVENT_SERVER_ERROR:
    case AURIX_EVENT_REJOIN_FAILED:
        printf("error %s: %s\n", aurix_event_code(ev), aurix_event_message(ev));
        break;
    case AURIX_EVENT_RECOVERING:
        printf("recovering (attempt %llu, next in %llu ms): %s\n",
               (unsigned long long)aurix_event_number(ev),
               (unsigned long long)aurix_event_number2(ev), aurix_event_message(ev));
        break;
    case AURIX_EVENT_RECOVERED:
        printf("recovered (resumed=%d)\n", (int)aurix_event_flag(ev));
        break;
    case AURIX_EVENT_FAILED_TO_RECOVER:
    case AURIX_EVENT_DISCONNECTED:
    case AURIX_EVENT_KICKED:
        printf("connection ended: %s\n", aurix_event_message(ev));
        rc = 1;
        break;
    default:
        /* Every event is also available as JSON for logging or forward compatibility. */
        printf("event %s\n", aurix_event_json(ev));
        break;
    }
    (void)client;
    aurix_event_free(ev);
    return rc;
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s <ws-url> <token> <channel-uuid> [seconds]\n", argv[0]);
        return 2;
    }
    unsigned seconds = argc > 4 ? (unsigned)atoi(argv[4]) : 5;

    struct AurixUuid channel;
    if (aurix_uuid_parse(argv[3], &channel) != AURIX_OK) {
        fprintf(stderr, "bad channel id: %s\n", aurix_last_error());
        return 2;
    }

    struct AurixClientConfig cfg;
    aurix_client_config_default(&cfg);
    cfg.ws_url = argv[1];
    cfg.token = argv[2];
    cfg.vad_gate = false; /* always send our test tone */
    aurix_dsp_config_bypass(&cfg.dsp); /* synthetic tone: keep NS/AGC from shaping it */
    cfg.dsp.high_pass = true;
    cfg.encoder.bitrate_bps = 24000;                   /* until the channel policy arrives */
    cfg.encoder.complexity = 5;                        /* cheap enough for a handheld */
    cfg.encoder.max_bandwidth = AURIX_BANDWIDTH_WIDEBAND;

    struct AurixClient *client = aurix_client_create(&cfg);
    if (!client) {
        fprintf(stderr, "create failed: %s\n", aurix_last_error());
        return 2;
    }
    printf("aurix_client %s\n", aurix_version());

    if (aurix_client_connect(client) != AURIX_OK) {
        fprintf(stderr, "connect failed: %s\n", aurix_last_error());
        aurix_client_destroy(client);
        return 3;
    }

    /* Connecting is asynchronous: wait for SESSION_READY (or a terminal failure) before joining. */
    for (;;) {
        struct AurixEvent *ev = aurix_client_wait_event(client, 15000);
        if (!ev) {
            fprintf(stderr, "connect failed: timed out waiting for the session\n");
            aurix_client_destroy(client);
            return 3;
        }
        enum AurixEventType type = aurix_event_type(ev);
        if (type == AURIX_EVENT_DISCONNECTED || type == AURIX_EVENT_FAILED_TO_RECOVER) {
            fprintf(stderr, "connect failed: %s\n", aurix_event_message(ev));
            aurix_event_free(ev);
            aurix_client_destroy(client);
            return 3;
        }
        int ready = type == AURIX_EVENT_SESSION_READY;
        handle_event(client, ev);
        if (ready) break;
    }

    uint64_t join_request = 0;
    if (aurix_client_join_channel(client, &channel, NULL, &join_request) != AURIX_OK) {
        fprintf(stderr, "join failed: %s\n", aurix_last_error());
        aurix_client_destroy(client);
        return 4;
    }

    /* 20 ms of a 440 Hz tone at 48 kHz mono, and a stereo output buffer of the same length. */
    float tone[AURIX_FRAME_SAMPLES];
    float out[AURIX_FRAME_SAMPLES * 2];
    double phase = 0.0;
    size_t active_frames = 0;
    int ended = 0;

    for (unsigned tick = 0; tick < seconds * 50 && !ended; tick++) {
        for (size_t i = 0; i < AURIX_FRAME_SAMPLES; i++) {
            tone[i] = (float)(0.3 * sin(phase));
            phase += TWO_PI * 440.0 / AURIX_SAMPLE_RATE;
        }
        aurix_client_push_capture_f32(client, tone, AURIX_FRAME_SAMPLES, AURIX_SAMPLE_RATE, 1);
        if (aurix_client_mix_output_f32(client, out, AURIX_FRAME_SAMPLES * 2, 2) > 0) active_frames++;

        struct AurixEvent *ev;
        while ((ev = aurix_client_poll_event(client)) != NULL) {
            if (handle_event(client, ev)) ended = 1;
        }
        sleep_ms(20);
    }

    struct AurixStats st;
    if (aurix_client_stats(client, &st) == AURIX_OK) {
        printf("sent %llu pkts, received %llu pkts (%llu audio frames), rtt %.1f ms, loss %.1f%%, "
               "mixed %zu active frames\n",
               (unsigned long long)st.packets_sent, (unsigned long long)st.packets_received,
               (unsigned long long)st.audio_frames_received, st.rtt_ms, st.loss_percent,
               active_frames);
    }
    struct AurixDspStats dsp;
    if (aurix_client_dsp_stats(client, &dsp) == AURIX_OK) {
        printf("dsp: speech %.2f, agc %.1f dB, aec %s (erle %.1f dB, delay %u ms)\n", dsp.speech_probability,
               dsp.agc_gain_db, dsp.echo_converged ? "converged" : "adapting", dsp.erle_db, dsp.echo_delay_ms);
    }

    aurix_client_leave_channel(client, &channel);
    aurix_client_disconnect(client);
    aurix_client_destroy(client);
    return 0;
}
