#include "InterpCommon.hlsli"

Texture2D<float4> prevPyr : register(t0);
Texture2D<float4> currPyr : register(t1);
Texture2D<float4> coarseFlow : register(t2);
StructuredBuffer<FrameDecision> decisionBuf : register(t3);

RWTexture2D<float4> outFlow : register(u0);

float EvalRefinePatch(Texture2D<float4> texRef, Texture2D<float4> texCmp, float2 uv, float2 offPx, float2 invSrc)
{
    static const float2 taps[5] = {
        float2( 0.0,  0.0),
        float2(-1.0,  0.0),
        float2( 1.0,  0.0),
        float2( 0.0, -1.0),
        float2( 0.0,  1.0)
    };

    float2 uvDelta = offPx * invSrc;
    float sum = 0.0;
    [unroll]
    for (int i = 0; i < 5; ++i)
    {
        float2 uRef = ClampActiveUv(uv + taps[i] * invSrc, srcSize, activeUvMin, activeUvMax);
        float2 uCmp = ClampActiveUv(uRef + uvDelta, srcSize, activeUvMin, activeUvMax);
        float4 a = texRef.SampleLevel(sampLinear, uRef, 0);
        float4 b = texCmp.SampleLevel(sampLinear, uCmp, 0);
        sum += abs(a.r - b.r) + gradWeight * (abs(a.g - b.g) + abs(a.b - b.b));
    }
    return sum * 0.2;
}

float2 RefineDirection(
    Texture2D<float4> texRef,
    Texture2D<float4> texCmp,
    float2 uv,
    float2 invSrc,
    float2 toLevel,
    float2 coarseInv,
    bool isForward,
    float centerGrad)
{
    static const float2 seedOffsets[5] = {
        float2( 0.0,  0.0),
        float2(-1.0,  0.0),
        float2( 1.0,  0.0),
        float2( 0.0, -1.0),
        float2( 0.0,  1.0)
    };

    float4 cCenterRaw = coarseFlow.SampleLevel(sampLinear, ClampUv(uv, coarseSize), 0);
    float2 cCenter = (isForward ? cCenterRaw.xy : cCenterRaw.zw) * toLevel;

    float2 bestMv = float2(0.0, 0.0);
    float zeroCost = EvalRefinePatch(texRef, texCmp, uv, float2(0.0, 0.0), invSrc);
    float bestCost = zeroCost;

    [unroll]
    for (int i = 0; i < 5; ++i)
    {
        float4 raw = coarseFlow.SampleLevel(sampLinear, ClampUv(uv + seedOffsets[i] * coarseInv, coarseSize), 0);
        float2 cand = (isForward ? raw.xy : raw.zw) * toLevel;
        float reg = smoothPrior * length(cand - cCenter) * 0.15;
        float cost = EvalRefinePatch(texRef, texCmp, uv, cand, invSrc) + reg;
        if (cost < bestCost)
        {
            bestCost = cost;
            bestMv = cand;
        }
    }

    float zeroBias = flatZeroBias * 0.42 * exp(-centerGrad * 16.0);
    float2 seedCenter = bestMv;

    // Step 1.0 level-pixel local neighborhood
    [unroll]
    for (int iy = -1; iy <= 1; ++iy)
    {
        [unroll]
        for (int ix = -1; ix <= 1; ++ix)
        {
            if (ix == 0 && iy == 0)
                continue;
            float2 cand = seedCenter + float2(ix, iy);
            float reg = zeroBias * length(cand) + smoothPrior * 0.08 * length(cand - cCenter);
            float cost = EvalRefinePatch(texRef, texCmp, uv, cand, invSrc) + reg;
            if (cost < bestCost)
            {
                bestCost = cost;
                bestMv = cand;
            }
        }
    }

    // Step 0.5 level-pixel subpixel refinement
    float2 halfCenter = bestMv;
    [unroll]
    for (int hy = -1; hy <= 1; ++hy)
    {
        [unroll]
        for (int hx = -1; hx <= 1; ++hx)
        {
            if (hx == 0 && hy == 0)
                continue;
            float2 cand = halfCenter + float2(hx, hy) * 0.5;
            float reg = zeroBias * length(cand) + smoothPrior * 0.08 * length(cand - cCenter);
            float cost = EvalRefinePatch(texRef, texCmp, uv, cand, invSrc) + reg;
            if (cost < bestCost)
            {
                bestCost = cost;
                bestMv = cand;
            }
        }
    }

    // Orientation-aware active-video subtitle lock (9:16 higher 0.62..0.94 vs 16:9 lower 0.74..0.98)
    float relY = (uv.y - activeUvMin.y) / max(activeUvMax.y - activeUvMin.y, 1e-3);
    if (subLockWeight > 0.0 && relY >= subBandStartY && relY <= subBandEndY && centerGrad > 0.04)
    {
        if (zeroCost <= bestCost * (1.0 + 0.16 * subLockWeight))
        {
            bestMv = float2(0.0, 0.0);
        }
    }

    return bestMv;
}

[numthreads(8, 8, 1)]
void CSFlowRefine(uint3 dtid : SV_DispatchThreadID)
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
    float2 toLevel = float2(srcSize) / float2(fullSize);
    float2 toFull = float2(fullSize) * invSrc;
    float2 coarseInv = 1.0 / float2(max(coarseSize, uint2(1, 1)));

    float gradP = prevPyr.SampleLevel(sampLinear, uv, 0).a;
    float gradC = currPyr.SampleLevel(sampLinear, uv, 0).a;

    float2 fwd = RefineDirection(prevPyr, currPyr, uv, invSrc, toLevel, coarseInv, true,  gradP) * toFull;
    float2 bwd = RefineDirection(currPyr, prevPyr, uv, invSrc, toLevel, coarseInv, false, gradC) * toFull;

    outFlow[dtid.xy] = float4(fwd, bwd);
}
