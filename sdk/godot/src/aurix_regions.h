// AurixRegions — region discovery helper (RefCounted). The HTTP round trips are the game's
// (Godot `HTTPRequest`); this class builds the discovery URL, parses the response, records RTT
// probes and ranks the endpoints with the same rules as the other SDKs.
#pragma once

#include "aurix_client.hpp"

#include <godot_cpp/classes/ref_counted.hpp>
#include <godot_cpp/variant/array.hpp>
#include <godot_cpp/variant/dictionary.hpp>
#include <godot_cpp/variant/string.hpp>

namespace godot {

class AurixRegions : public RefCounted {
    GDCLASS(AurixRegions, RefCounted)

public:
    /// `GET` URL for `/v1/me/regions`; `latitude`/`longitude` are ignored unless `has_location`.
    static String discovery_url(const String& api_url, const String& preferred_region, bool has_location,
                                double latitude, double longitude);

    /// Load a `/v1/me/regions` (or `/v1/regions`) response body. False on malformed input.
    bool parse(const String& json);
    bool is_valid() const;
    int size() const;
    Dictionary get_endpoint(int index) const;
    Array get_all() const;
    /// Best RTT sample in ms for entry `index`; negative when every probe failed.
    int set_rtt(int index, double rtt_ms);
    /// Re-rank in place (RTT, then preferred region, distance and load). `rtt_tolerance_ms <= 0` = default.
    int rank(const String& preferred_region, double rtt_tolerance_ms);
    /// `ws_url` of the first entry after ranking, or "" when the list is empty.
    String best_ws_url() const;

protected:
    static void _bind_methods();

private:
    aurix::Regions regions_;
};

}  // namespace godot
