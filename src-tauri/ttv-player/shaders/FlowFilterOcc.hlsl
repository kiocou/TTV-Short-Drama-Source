#include "InterpCommon.hlsli"

Texture2D<float4> prevPyr : register(t0);
Texture2D<float4> currPyr : register(t1);
Texture2D<float4> rawFlow : register(t2);
StructuredBuffer<FrameDecision> decisionBuf : register(t3);

RWTexture2D<float4> outCleanFlow : register(u0);
RWTexture2D<float4> outOccConf   : register(u1);

[numthreads(8, 8, 1)]
void CSFlowFilterOcc(uint3 dtid : SV_DispatchThreadID)
{
    if (dtid.x >= srcSize.x || dtid.y >= srcSize.y)
        return;

    uint flags = decisionBuf[0].flags;
    if ((flags & FD_STATIC_PANEL) != 0u || (flags & FD_GLOBAL_FADE) != 0u)
    {
        outCleanFlow[dtid.xy] = float4(0.0, 0.0, 0.0, 0.0);
        outOccConf[dtid.xy]   = float4(1.0, 1.0, 1.0, 1.0);
        return;
    }
    if ((flags & FD_SCENE_CUT) != 0u)
    {
        outCleanFlow[dtid.xy] = float4(0.0, 0.0, 0.0, 0.0);
        outOccConf[dtid.xy]   = float4(0.0, 0.0, 0.0, 0.0);
        return;
    }

    float2 invSrc = 1.0 / float2(srcSize);
    float2 invFull = 1.0 / float2(fullSize);
    float2 uv = (float2(dtid.xy) + 0.5) * invSrc;
    if (IsOutsideActiveUv(uv, activeUvMin, activeUvMax))
    {
        outCleanFlow[dtid.xy] = float4(0.0, 0.0, 0.0, 0.0);
        outOccConf[dtid.xy]   = float4(1.0, 1.0, 1.0, 0.0);
        return;
    }

    float4 centerFlow = rawFlow.SampleLevel(sampPoint, uv, 0);
    {
        float4 nbSamples[9];
        int k = 0;
        [unroll]
        for (int oy = -1; oy <= 1; ++oy)
        {
            [unroll]
            for (int ox = -1; ox <= 1; ++ox)
            {
                float2 nuv = ClampActiveUv(uv + float2(ox, oy) * invSrc, srcSize, activeUvMin, activeUvMax);
                nbSamples[k++] = rawFlow.SampleLevel(sampPoint, nuv, 0);
            }
        }
        float threshF = max(2.2, length(centerFlow.xy) * 0.25);
        float threshB = max(2.2, length(centerFlow.zw) * 0.25);
        int agreeF = 0;
        int agreeB = 0;
        [unroll]
        for (int j = 0; j < 9; ++j)
        {
            if (j != 4)
            {
                if (length(nbSamples[j].xy - centerFlow.xy) <= threshF) { agreeF += 1; }
                if (length(nbSamples[j].zw - centerFlow.zw) <= threshB) { agreeB += 1; }
            }
        }
        // Only reject isolated spikes/outliers (< 2 supporting neighbors) so thin
        // true moving features stay crisp while jello outlier bubbles snap to 3x3 consensus.
        if (agreeF < 2)
        {
            float bestCostF = 1e30;
            float2 medF = centerFlow.xy;
            [unroll]
            for (int a = 0; a < 9; ++a)
            {
                float cost = 0.0;
                [unroll]
                for (int b = 0; b < 9; ++b)
                {
                    cost += length(nbSamples[a].xy - nbSamples[b].xy);
                }
                if (cost < bestCostF)
                {
                    bestCostF = cost;
                    medF = nbSamples[a].xy;
                }
            }
            centerFlow.xy = medF;
        }
        if (agreeB < 2)
        {
            float bestCostB = 1e30;
            float2 medB = centerFlow.zw;
            [unroll]
            for (int a = 0; a < 9; ++a)
            {
                float cost = 0.0;
                [unroll]
                for (int b = 0; b < 9; ++b)
                {
                    cost += length(nbSamples[a].zw - nbSamples[b].zw);
                }
                if (cost < bestCostB)
                {
                    bestCostB = cost;
                    medB = nbSamples[a].zw;
                }
            }
            centerFlow.zw = medB;
        }
    }
    float lumaP = prevPyr.SampleLevel(sampLinear, uv, 0).r;
    float lumaC = currPyr.SampleLevel(sampLinear, uv, 0).r;

    // 3x3 joint edge-preserving bilateral filter (keeps ink outlines and comic borders sharp)
    float2 sumF = float2(0.0, 0.0);
    float wSumF = 0.0;
    float2 sumB = float2(0.0, 0.0);
    float wSumB = 0.0;

    float edgeGain = edgeSharpness * 120.0;

    [unroll]
    for (int dy = -1; dy <= 1; ++dy)
    {
        [unroll]
        for (int dx = -1; dx <= 1; ++dx)
        {
            float2 nbUv = ClampActiveUv(uv + float2(dx, dy) * invSrc, srcSize, activeUvMin, activeUvMax);
            float4 nbFlow = rawFlow.SampleLevel(sampPoint, nbUv, 0);
            float nbLp = prevPyr.SampleLevel(sampLinear, nbUv, 0).r;
            float nbLc = currPyr.SampleLevel(sampLinear, nbUv, 0).r;

            float spatialW = (dx == 0 && dy == 0) ? 1.0 : ((abs(dx) + abs(dy) == 1) ? 0.65 : 0.40);

            float dLp = nbLp - lumaP;
            float2 dVf = nbFlow.xy - centerFlow.xy;
            float wf = spatialW * exp(-dLp * dLp * (edgeGain * 1.35) - dot(dVf, dVf) * 0.045);
            sumF += nbFlow.xy * wf;
            wSumF += wf;

            float dLc = nbLc - lumaC;
            float2 dVb = nbFlow.zw - centerFlow.zw;
            float wb = spatialW * exp(-dLc * dLc * (edgeGain * 1.35) - dot(dVb, dVb) * 0.045);
            sumB += nbFlow.zw * wb;
            wSumB += wb;
        }
    }

    float2 vf = sumF / max(wSumF, 1e-5);
    float2 vb = sumB / max(wSumB, 1e-5);
    float gFlat = max(prevPyr.SampleLevel(sampLinear, uv, 0).a, currPyr.SampleLevel(sampLinear, uv, 0).a);
    float lDiffFlat = abs(lumaP - lumaC);
    if (gFlat < 0.035 && lDiffFlat < 0.025 && max(dot(vf, vf), dot(vb, vb)) < 0.64)
    {
        float keep = smoothstep(0.008, 0.025, lDiffFlat);
        vf *= keep;
        vb *= keep;
    }

    // Clamp vectors near active video borders to prevent letterbox/pillarbox bleeding
    float2 edgeDist = min(uv - activeUvMin, activeUvMax - uv) * float2(fullSize);
    float borderWeight = saturate(min(edgeDist.x, edgeDist.y) / 12.0);
    vf *= lerp(0.5, 1.0, borderWeight);
    vb *= lerp(0.5, 1.0, borderWeight);

    // Protect crisp stationary strokes in the orientation-specific active subtitle band
    float relY = (uv.y - activeUvMin.y) / max(activeUvMax.y - activeUvMin.y, 1e-3);
    if (subLockWeight > 0.01 && relY >= subBandStartY && relY <= subBandEndY)
    {
        float gPrev = prevPyr.SampleLevel(sampLinear, uv, 0).a;
        float gCurr = currPyr.SampleLevel(sampLinear, uv, 0).a;
        float stableStroke = smoothstep(0.05, 0.14, min(gPrev, gCurr)) * saturate(1.0 - abs(lumaP - lumaC) * 18.0);
        float rampIn = smoothstep(subBandStartY, min(subBandStartY + 0.06, subBandEndY), relY);
        float rampOut = 1.0 - smoothstep(max(subBandStartY, subBandEndY - 0.03), subBandEndY + 0.02, relY);
        float subMask = saturate(rampIn * rampOut) * stableStroke * saturate(subLockWeight);
        float damp = 1.0 - subMask * 0.92;
        vf *= damp;
        vb *= damp;
    }

    // Forward-backward consistency & occlusion detection
    float2 destF = ClampActiveUv(uv + vf * invFull, srcSize, activeUvMin, activeUvMax);
    float2 vbAtDest = rawFlow.SampleLevel(sampLinear, destF, 0).zw;
    float fbErrF = length(vf + vbAtDest);
    float threshF = max(2.0, 0.28 * length(vf)) * occStrictness;
    float fwdOcc = saturate(1.0 - max(0.0, fbErrF - threshF) / max(threshF, 1.0));

    float2 destB = ClampActiveUv(uv + vb * invFull, srcSize, activeUvMin, activeUvMax);
    float2 vfAtDest = rawFlow.SampleLevel(sampLinear, destB, 0).xy;
    float fbErrB = length(vb + vfAtDest);
    float threshB = max(2.0, 0.28 * length(vb)) * occStrictness;
    float bwdOcc = saturate(1.0 - max(0.0, fbErrB - threshB) / max(threshB, 1.0));

    float photoErrF = abs(lumaP - currPyr.SampleLevel(sampLinear, destF, 0).r);
    float photoErrB = abs(lumaC - prevPyr.SampleLevel(sampLinear, destB, 0).r);
    float photoConf = saturate(1.0 - max(0.0, min(photoErrF, photoErrB) - 0.05) * 6.0);

    float flowMag = max(length(vf), length(vb));
    float magSafety = saturate(1.0 - max(0.0, flowMag - maxSafeFlowPx) / max(maxSafeFlowPx * 0.5, 4.0));
    float warpSafety = saturate(max(fwdOcc, bwdOcc) * photoConf * magSafety);

    outCleanFlow[dtid.xy] = float4(vf, vb);
    outOccConf[dtid.xy]   = float4(fwdOcc, bwdOcc, photoConf, warpSafety);
}
