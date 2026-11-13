// Cache-free reference transport: no radiance cache, no ReSTIR, no firefly
// clamp. Used to validate energy and to judge the accelerated consumer; the
// variant selection lives in pt_render.slang (PT_MODE).
#define PT_MODE 1
#include "pt_render.slang"
