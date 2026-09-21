#!/usr/bin/env bash
# Shape the traffic of one Aurix media port (UDP: AURX, QUIC, WebRTC) on a local interface with
# Linux netem, so a lossy / jittery WAN can be reproduced on a developer box or a CI runner
# between a native client and a node listening on loopback.
#
# Direction is by port: `down` shapes what the node sends (source port = media port, i.e. the
# clients' downlink), `up` what the clients send to it (destination port = media port). Both
# take a netem parameter list verbatim, e.g. "loss 20%", "delay 40ms 20ms", "loss 12% delay 20ms
# 10ms reorder 25% 50%" (reordering needs a delay; see `man tc-netem`). Everything else on the
# interface — the WebSocket control plane, REST, other ports — is left alone.
#
# Usage:
#   tools/netem/shape.sh apply <media-port> [--down "<netem args>"] [--up "<netem args>"]
#   tools/netem/shape.sh show
#   tools/netem/shape.sh clear
#
# Environment:
#   AURIX_NETEM_DEV   interface (default lo)
#
# `apply` replaces whatever shaping this script installed before (one media port at a time;
# IPv4 only). Requires CAP_NET_ADMIN — run it with sudo. Always `clear` afterwards: the qdisc is
# host-global and outlives the shell.
set -euo pipefail

DEV="${AURIX_NETEM_DEV:-lo}"

usage() {
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 64
}

clear_all() {
    tc qdisc del dev "$DEV" root 2>/dev/null || true
}

apply() {
    local port="" down="" up=""
    port="${1:-}"
    [[ "$port" =~ ^[0-9]+$ ]] || usage
    shift
    while (($# > 0)); do
        case "$1" in
            --down)
                down="${2:-}"
                shift 2
                ;;
            --up)
                up="${2:-}"
                shift 2
                ;;
            *) usage ;;
        esac
    done
    [[ -n "$down" || -n "$up" ]] || usage

    clear_all
    # Band 1:1 takes every packet (priomap all zero); the filters below steer the media port
    # into 1:2 (downlink) and 1:3 (uplink), where netem sits. 1:4 stays an idle pfifo.
    tc qdisc add dev "$DEV" root handle 1: prio bands 4 priomap 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
    local -a args
    if [[ -n "$down" ]]; then
        read -r -a args <<<"$down"
        tc qdisc add dev "$DEV" parent 1:2 handle 20: netem "${args[@]}"
        tc filter add dev "$DEV" parent 1: protocol ip prio 1 u32 \
            match ip protocol 17 0xff match ip sport "$port" 0xffff flowid 1:2
    fi
    if [[ -n "$up" ]]; then
        read -r -a args <<<"$up"
        tc qdisc add dev "$DEV" parent 1:3 handle 30: netem "${args[@]}"
        tc filter add dev "$DEV" parent 1: protocol ip prio 2 u32 \
            match ip protocol 17 0xff match ip dport "$port" 0xffff flowid 1:3
    fi
}

case "${1:-}" in
    apply)
        shift
        apply "$@"
        ;;
    show)
        tc -s qdisc show dev "$DEV"
        tc filter show dev "$DEV"
        ;;
    clear) clear_all ;;
    *) usage ;;
esac
