#include "InterpCommon.hlsli"

Texture2D<float4> prevPyr : register(t0);
Texture2D<float4> currPyr : register(t1);
StructuredBuffer<FrameDecision> decisionBuf : register(t2);

RWTexture2D<float4> outFlow : register(u0);

float PatchCost(Texture2D<float4> texRef, Texture2D<float4> texCmp, float2 uvBase, float2 uvDelta, float2 texelUv)
{
    static const float2 offsets[5] = {
        float2( 0.0,  0.0),
        float2(-1.0, -1.0),
        float2( 1.0, -1.0),
        float2(-1.0,  1.0),
        float2( 1.0,  1.0)
    };

    float cost = 0.0;
    [unroll]
    for (int i = 0; i < 5; ++i)
    {
        float2 uRef = ClampActiveUv(uvBase + offsets[i] * texelUv, srcSize, activeUvMin, activeUvMax);
        float2 uCmp = ClampActiveUv(uRef + uvDelta, srcSize, activeUvMin, activeUvMax);
        float4 a = texRef.SampleLevel(sampLinear, uRef, 0);
        float4 b = texCmp.SampleLevel(sampLinear, uCmp, 0);
        cost += abs(a.r - b.r) + gradWeight * (abs(a.g - b.g) + abs(a.b - b.b));
    }
    return cost * 0.2;
}

float2 SearchBestOffset(Texture2D<float4> texRef, Texture2D<float4> texCmp, float2 uv, float2 texelUv, float zeroPenaltyScale)
{
    float2 bestD = float2(0.0, 0.0);
    float bestCost = PatchCost(texRef, texCmp, uv, float2(0.0, 0.0), texelUv);

    [loop]
    for (int dy = -4; dy <= 4; ++dy)
    {
        [unroll]
        for (int dx = -4; dx <= 4; ++dx)
        {
            if (dx == 0 && dy == 0)
                continue;
            float2 d = float2(dx, dy);
            float c = PatchCost(texRef, texCmp, uv, d * texelUv, texelUv) + length(d) * zeroPenaltyScale;
            if (c < bestCost)
            {
                bestCost = c;
                bestD = d;
            }
        }
    }

    // Outer wide-motion ring (+-6 and +-8 coarse pixels => up to +-128 full-res px)
    static const float2 wideOffsets[12] = {
        float2(-6,  0), float2( 6,  0), float2( 0, -6), float2( 0,  6),
        float2(-6, -6), float2( 6, -6), float2(-6,  6), float2( 6,  6),
        float2(-8,  0), float2( 8,  0), float2( 0, -8), float2( 0,  8)
    };
    [unroll]
    for (int i = 0; i < 12; ++i)
    {
        float2 d = wideOffsets[i];
        float c = PatchCost(texRef, texCmp, uv, d * texelUv, texelUv) + length(d) * zeroPenaltyScale;
        if (c < bestCost)
        {
            bestCost = c;
            bestD = d;
        }
    }

    return bestD;
}

[numthreads(8, 8, 1)]
void CSFlowCoarse(uint3 dtid : SV_DispatchThreadID)
{
    if (dtid.x >= srcSize.x || dtid.y >= srcSize.y)
        return;

    uint flags = decisionBuf[0].flags;
    if ((flags & (FD_STATIC_PANEL | FD_SCENE_CUT)) != 0u)
    {
        outFlow[dtid.xy] = float4(0.0, 0.0, 0.0, 0.0);
        return;
    }

    float2 invSrc = 1.0 / float2(srcSize);
    float2 uv = (float2(dtid.xy) + 0.5) * invSrc;
    if (IsOutsideActiveUv(uv, activeUvMin, activeUvMax))
    {
        outFlow[dtid.xy] = float4(0.0, 0.0, 0.0, 0.0);
        return;
    }
    float2 toFull = float2(fullSize) * invSrc;

    float gradP = prevPyr.SampleLevel(sampLinear, uv, 0).a;
    float gradC = currPyr.SampleLevel(sampLinear, uv, 0).a;

    float zeroBiasF = 0.0015 + flatZeroBias * exp(-gradP * 18.0);
    float zeroBiasB = 0.0015 + flatZeroBias * exp(-gradC * 18.0);

    float2 fwd = SearchBestOffset(prevPyr, currPyr, uv, invSrc, zeroBiasF) * toFull;
    float2 bwd = SearchBestOffset(currPyr, prevPyr, uv, invSrc, zeroBiasB) * toFull;

    outFlow[dtid.xy] = float4(fwd, bwd);
}
