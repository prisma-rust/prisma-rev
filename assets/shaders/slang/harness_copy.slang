// Minimal compute kernel used by the reusable GPU shader harness test.
//
// It deliberately exercises the three pieces the harness must wire up:
//   * a read-only storage buffer  (binding 0)
//   * a read-write storage buffer (binding 1)
//   * a push-constant block       (count / mul / add)
//
// The host dispatches 4x1x1 groups of 4 threads, writes input[0..count],
// and checks output[i] == input[i] * mul + add.

struct HarnessPush
{
    uint count;
    uint mul;
    uint add;
};

[[vk::binding(0, 0)]]
StructuredBuffer<uint> inputData;

[[vk::binding(1, 0)]]
RWStructuredBuffer<uint> outputData;

[[vk::push_constant]]
HarnessPush params;

[shader("compute")]
[numthreads(4, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint index = tid.x;
    if (index >= params.count)
        return;

    outputData[index] = inputData[index] * params.mul + params.add;
}
