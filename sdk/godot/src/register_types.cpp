#include "aurix_participant_player.h"
#include "aurix_regions.h"
#include "aurix_voice_client.h"

#include <gdextension_interface.h>
#include <godot_cpp/core/class_db.hpp>
#include <godot_cpp/core/defs.hpp>
#include <godot_cpp/godot.hpp>

using namespace godot;

namespace {

void initialize_aurix_voice(ModuleInitializationLevel level) {
    if (level != MODULE_INITIALIZATION_LEVEL_SCENE) return;
    GDREGISTER_CLASS(AurixVoiceClient);
    GDREGISTER_CLASS(AurixParticipantPlayer);
    GDREGISTER_CLASS(AurixRegions);
}

void uninitialize_aurix_voice(ModuleInitializationLevel level) {
    if (level != MODULE_INITIALIZATION_LEVEL_SCENE) return;
}

}  // namespace

extern "C" {
GDExtensionBool GDE_EXPORT aurix_voice_library_init(GDExtensionInterfaceGetProcAddress p_get_proc_address,
                                                    GDExtensionClassLibraryPtr p_library,
                                                    GDExtensionInitialization* r_initialization) {
    GDExtensionBinding::InitObject init(p_get_proc_address, p_library, r_initialization);
    init.register_initializer(initialize_aurix_voice);
    init.register_terminator(uninitialize_aurix_voice);
    init.set_minimum_library_initialization_level(MODULE_INITIALIZATION_LEVEL_SCENE);
    return init.init();
}
}
