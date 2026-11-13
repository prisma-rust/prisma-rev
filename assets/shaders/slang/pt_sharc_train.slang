// Standalone SHaRC training: traces paths and deposits radiance into the
// cache, never terminates on a cache lookup and writes no screen-space output.
// The graph dispatches it at its own resolution before resolve/consume; the
// variant selection lives in pt_render.slang (PT_MODE).
#define PT_MODE 2
#include "pt_render.slang"
