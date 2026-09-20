#include "aurix_regions.h"

#include "aurix_conversions.h"

#include <godot_cpp/core/class_db.hpp>

using namespace aurix_godot;

namespace godot {

String AurixRegions::discovery_url(const String& api_url, const String& preferred_region, bool has_location,
                                   double latitude, double longitude) {
    const std::string url = aurix::Regions::discovery_url(api_url.utf8().get_data(), preferred_region.utf8().get_data(),
                                                          has_location ? &latitude : nullptr,
                                                          has_location ? &longitude : nullptr);
    return String::utf8(url.c_str());
}

bool AurixRegions::parse(const String& json) {
    regions_ = aurix::Regions::parse(json.utf8().get_data());
    return regions_.valid();
}

bool AurixRegions::is_valid() const { return regions_.valid(); }

int AurixRegions::size() const { return regions_.valid() ? static_cast<int>(regions_.size()) : 0; }

Dictionary AurixRegions::get_endpoint(int index) const {
    AurixRegionEndpoint r;
    if (!regions_.valid() || index < 0 || !regions_.get(static_cast<std::size_t>(index), r)) return Dictionary();
    return region_to_dict(r);
}

Array AurixRegions::get_all() const {
    Array out;
    if (!regions_.valid()) return out;
    for (const AurixRegionEndpoint& r : regions_.all()) out.push_back(region_to_dict(r));
    return out;
}

int AurixRegions::set_rtt(int index, double rtt_ms) {
    if (!regions_.valid() || index < 0) return AURIX_INVALID_ARGUMENT;
    return regions_.set_rtt(static_cast<std::size_t>(index), rtt_ms);
}

int AurixRegions::rank(const String& preferred_region, double rtt_tolerance_ms) {
    if (!regions_.valid()) return AURIX_INVALID_ARGUMENT;
    return regions_.rank(preferred_region.utf8().get_data(), rtt_tolerance_ms);
}

String AurixRegions::best_ws_url() const {
    AurixRegionEndpoint r;
    if (!regions_.valid() || regions_.size() == 0 || !regions_.get(0, r)) return String();
    return cstr(r.ws_url);
}

void AurixRegions::_bind_methods() {
    ClassDB::bind_static_method("AurixRegions", D_METHOD("discovery_url", "api_url", "preferred_region", "has_location", "latitude", "longitude"),
                                &AurixRegions::discovery_url, DEFVAL(String()), DEFVAL(false), DEFVAL(0.0), DEFVAL(0.0));
    ClassDB::bind_method(D_METHOD("parse", "json"), &AurixRegions::parse);
    ClassDB::bind_method(D_METHOD("is_valid"), &AurixRegions::is_valid);
    ClassDB::bind_method(D_METHOD("size"), &AurixRegions::size);
    ClassDB::bind_method(D_METHOD("get_endpoint", "index"), &AurixRegions::get_endpoint);
    ClassDB::bind_method(D_METHOD("get_all"), &AurixRegions::get_all);
    ClassDB::bind_method(D_METHOD("set_rtt", "index", "rtt_ms"), &AurixRegions::set_rtt);
    ClassDB::bind_method(D_METHOD("rank", "preferred_region", "rtt_tolerance_ms"), &AurixRegions::rank, DEFVAL(String()), DEFVAL(0.0));
    ClassDB::bind_method(D_METHOD("best_ws_url"), &AurixRegions::best_ws_url);
}

}  // namespace godot
