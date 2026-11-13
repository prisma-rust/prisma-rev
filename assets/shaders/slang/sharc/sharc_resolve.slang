#include "sharc_common.slang"

struct SharcResolvePush
{
    float4 camera_pos;       // .xyz = camera position
    float4 camera_pos_prev;  // .xyz = camera position prev
    float  scene_scale;
    uint   capacity;
    uint   accumulation_frame_num;
    uint   stale_frame_num_max;
    uint   frame_index;
    float  radiance_scale;
    uint   enable_anti_firefly;
    uint   pad;
};

[[vk::push_constant]]
SharcResolvePush push;

[[vk::binding(0, 0)]]
RWStructuredBuffer<HashGridKey> u_sharc_hash_entries;

[[vk::binding(1, 0)]]
RWStructuredBuffer<SharcAccumulationData> u_sharc_accumulation;

[[vk::binding(2, 0)]]
RWStructuredBuffer<SharcPackedData> u_sharc_resolved;

[numthreads(256, 1, 1)]
void resolveMain(uint3 did : SV_DispatchThreadID)
{
    uint entry_index = did.x;
    if (entry_index >= push.capacity)
        return;

    SharcParameters params;
    params.gridParameters.cameraPosition = push.camera_pos.xyz;
    params.gridParameters.logarithmBase = SHARC_GRID_LOGARITHM_BASE;
    params.gridParameters.sceneScale = push.scene_scale;
    params.gridParameters.levelBias = SHARC_GRID_LEVEL_BIAS;

    params.hashMapData.capacity = push.capacity;
    params.hashMapData.hashEntriesBuffer = u_sharc_hash_entries;

    params.accumulationBuffer = u_sharc_accumulation;
    params.resolvedBuffer = u_sharc_resolved;
    params.radianceScale = push.radiance_scale > 0.0 ? push.radiance_scale : 1000.0;
    params.enableAntiFireflyFilter = push.enable_anti_firefly != 0;

    SharcResolveParameters resolve_params;
    resolve_params.cameraPositionPrev = push.camera_pos_prev.xyz;
    resolve_params.accumulationFrameNum = push.accumulation_frame_num;
    resolve_params.staleFrameNumMax = push.stale_frame_num_max;
    resolve_params.frameIndex = push.frame_index;

    SharcResolveEntry(entry_index, params, resolve_params);
}
