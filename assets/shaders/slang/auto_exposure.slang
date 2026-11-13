// Automatic exposure metering for the tonemapper.
//
// Ported from RTXGI's `Samples/Shared/Shaders/AutoExposure.hlsl` (v2.9.0). The
// scene is rendered with physically-based radiance, so a fixed exposure cannot
// work across scenes: a corridor lit only by indirect bounces is ~47x darker
// than the same geometry under direct sun. Metering adapts the tonemapper to
// whatever is actually on screen.
//
// Two dispatches run back to back:
//   `luminanceHistogram`  - one 16x16 group per tile builds a log2 luminance
//                           histogram in group shared memory, then merges it
//                           into the global histogram with atomics.
//   `exposureAdaptation`  - a single thread reduces the histogram to the
//                           geometric mean of the pixels between two
//                           percentiles (rejecting black pixels and fireflies)
//                           and moves the adapted luminance toward it.
//
// The adapted luminance is stored as a float bit pattern in one R32_UINT
// element so the tonemapper reads it with a single scalar fetch and no
// readback. `AUTO_EXPOSURE_KEY` is the scene luminance that maps to middle
// grey: `exposure = AUTO_EXPOSURE_KEY / adaptedLuminance`.
//
// Both entry points share one descriptor set so the pipeline layout does not
// depend on which stage is dispatched:
//   set 0, binding 0 - source HDR colour (SAMPLED_IMAGE)
//   set 0, binding 1 - luminance histogram (STORAGE_BUFFER, R32_UINT x 256)
//   set 0, binding 2 - adapted luminance (STORAGE_BUFFER, R32_UINT x 1)
//
// Compile:
//   slangc auto_exposure.slang -profile spirv_1_5 -target spirv \
//          -entry luminanceHistogram -stage compute -fvk-use-entrypoint-name \
//          -o auto_exposure_hist.comp.spv
//   slangc auto_exposure.slang -profile spirv_1_5 -target spirv \
//          -entry exposureAdaptation -stage compute -fvk-use-entrypoint-name \
//          -o auto_exposure_adapt.comp.spv

static const uint AUTO_EXPOSURE_HISTOGRAM_BINS = 256u;
static const uint AUTO_EXPOSURE_GROUP_SIZE = 16u;

// Scene luminance mapped to middle grey (RTXGI `AutoExposureCb.h`).
static const float AUTO_EXPOSURE_KEY = 0.18;

// Pixels are split between the two nearest bins with this many fractional bits
// so bin quantization does not bias the mean. The scale cancels out in the
// reduction ratio, so only the weights' relative size matters.
static const uint FIXED_POINT_FRACTION_BITS = 6u;
static const float FIXED_POINT_SCALE = float(1u << FIXED_POINT_FRACTION_BITS);

struct AutoExposurePush
{
    // Meters `sourceSize` pixels; the dispatch is ceil(size / GROUP_SIZE).
    uint2 sourceSize;
    // Log2 luminance range the histogram resolves: bin 0 is `minLogLuminance`,
    // the last bin is `minLogLuminance + logLuminanceRange`.
    float minLogLuminance;
    float logLuminanceRange;
    float recipLogLuminanceRange;
    // Fraction of pixels excluded from the dark end / metered up to.
    float lowPercentile;
    float highPercentile;
    // Exponential approach rates in EV per second: toward a brighter target /
    // toward a darker target.
    float adaptationSpeedUp;
    float adaptationSpeedDown;
    float frameTime;
    // Snap to the metered value instead of adapting (first frame, camera cut).
    uint resetAdaptation;
    // The manual camera exposure, which the scene shaders already multiplied
    // into the HDR. Metering divides it back out so the adapted luminance
    // describes the scene, not the current slider position, and the slider
    // keeps acting as a gain on top of the automatic one.
    float exposureScale;
};
[[vk::push_constant]]
AutoExposurePush pc;

[[vk::binding(0, 0)]]
Texture2D<float4> sourceColor;

[[vk::binding(1, 0)]]
RWStructuredBuffer<uint> histogramOutput;

[[vk::binding(2, 0)]]
RWStructuredBuffer<uint> adaptedLuminance;

// Rec. 709 primaries, matching the linear sRGB working space.
float luminance_of(float3 color)
{
    return dot(color, float3(0.2126f, 0.7152f, 0.0722f));
}

groupshared uint s_Histogram[AUTO_EXPOSURE_HISTOGRAM_BINS];

[numthreads(AUTO_EXPOSURE_GROUP_SIZE, AUTO_EXPOSURE_GROUP_SIZE, 1)]
void luminanceHistogram(uint groupIndex : SV_GroupIndex, uint2 pixel : SV_DispatchThreadID)
{
    if (groupIndex < AUTO_EXPOSURE_HISTOGRAM_BINS)
        s_Histogram[groupIndex] = 0u;

    GroupMemoryBarrierWithGroupSync();

    if (all(pixel < pc.sourceSize))
    {
        // The scene shaders already applied the manual camera exposure, so
        // divide it back out to meter the scene rather than the slider.
        const float manualExposure = pc.exposureScale > 0.0f ? pc.exposureScale : 1.0f;
        // Denoiser and cache outputs can contain NaN or negative values; NaN is
        // skipped, negatives count as black.
        const float lum = max(
            luminance_of(sourceColor.Load(int3(pixel, 0)).rgb) / manualExposure, 0.0f);
        if (isfinite(lum))
        {
            const float normalizedLog = saturate(
                (log2(max(lum, 1e-8)) - pc.minLogLuminance) * pc.recipLogLuminanceRange);
            const float bin = normalizedLog * float(AUTO_EXPOSURE_HISTOGRAM_BINS - 1u);
            const uint lowerBin = uint(bin);
            const uint upperWeight = uint(frac(bin) * FIXED_POINT_SCALE);
            const uint lowerWeight = uint(FIXED_POINT_SCALE) - upperWeight;

            InterlockedAdd(s_Histogram[lowerBin], lowerWeight);
            // upperWeight != 0 implies lowerBin + 1 is still a valid bin.
            if (upperWeight != 0u && lowerBin + 1u < AUTO_EXPOSURE_HISTOGRAM_BINS)
                InterlockedAdd(s_Histogram[lowerBin + 1u], upperWeight);
        }
    }

    GroupMemoryBarrierWithGroupSync();

    if (groupIndex < AUTO_EXPOSURE_HISTOGRAM_BINS)
    {
        const uint count = s_Histogram[groupIndex];
        if (count != 0u)
            InterlockedAdd(histogramOutput[groupIndex], count);
    }
}

[numthreads(1, 1, 1)]
void exposureAdaptation()
{
    float pixelCount = 0.0f;
    for (uint bin = 0u; bin < AUTO_EXPOSURE_HISTOGRAM_BINS; ++bin)
        pixelCount += float(histogramOutput[bin]);

    // Nothing finite was metered; keep the previous value.
    if (pixelCount <= 0.0f)
        return;

    // Geometric mean of the pixels inside the percentile window. Bins straddling
    // a window edge contribute the fraction of their pixels that falls inside.
    const float lowCount = pixelCount * pc.lowPercentile;
    const float highCount = pixelCount * pc.highPercentile;
    const float binLogStep = pc.logLuminanceRange / float(AUTO_EXPOSURE_HISTOGRAM_BINS - 1u);

    float cumulativeCount = 0.0f;
    float weightedLogSum = 0.0f;
    float weightSum = 0.0f;
    for (uint bin = 0u; bin < AUTO_EXPOSURE_HISTOGRAM_BINS; ++bin)
    {
        const float binCount = float(histogramOutput[bin]);
        const float weight = max(
            min(cumulativeCount + binCount, highCount) - max(cumulativeCount, lowCount), 0.0f);
        weightedLogSum += weight * (pc.minLogLuminance + float(bin) * binLogStep);
        weightSum += weight;
        cumulativeCount += binCount;
    }

    if (weightSum <= 0.0f)
        return;

    const float targetLogLuminance = weightedLogSum / weightSum;
    float logLuminance = targetLogLuminance;

    const float previousLuminance = asfloat(adaptedLuminance[0]);
    const bool hasHistory = pc.resetAdaptation == 0u
        && isfinite(previousLuminance) && previousLuminance > 0.0f;
    if (hasHistory)
    {
        // Exponential approach in log space so the adaptation rate is constant
        // in EV rather than in luminance.
        const float previousLogLuminance = log2(previousLuminance);
        const float speed = targetLogLuminance > previousLogLuminance
            ? pc.adaptationSpeedUp : pc.adaptationSpeedDown;
        logLuminance = lerp(targetLogLuminance, previousLogLuminance,
            exp2(-pc.frameTime * speed));
    }

    adaptedLuminance[0] = asuint(exp2(logLuminance));
}
