// Grid-stride benchmark kernel used by the best-of-N harness test.
//
// Every invocation processes indices with a stride equal to the total number
// of dispatched threads, so different group counts are all correct. The outer
// repeat count gives the GPU enough work for the timing to be meaningful while
// keeping the test cheap enough for software Vulkan in CI.

struct BenchPush
{
    uint count;
    uint repeat;
    uint stride;
    uint pad;
};

[[vk::binding(0, 0)]]
StructuredBuffer<float> inputData;

[[vk::binding(1, 0)]]
RWStructuredBuffer<float> outputData;

[[vk::push_constant]]
BenchPush params;

[shader("compute")]
[numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    for (uint index = tid.x; index < params.count; index += params.stride)
    {
        float value = inputData[index];
        for (uint i = 0; i < params.repeat; ++i)
        {
            // Deliberately separate operations: the CPU reference below uses
            // the same IEEE multiply/add order, avoiding FMA differences.
            value = value * 1.5;
            value = value + 0.25;
        }
        outputData[index] = value;
    }
}
