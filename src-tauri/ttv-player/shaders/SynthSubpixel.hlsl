#include "InterpCommon.hlsli"

Texture2D<float4> prevColor : register(t0);
Texture2D<float4> currColor : register(t1);
Texture2D<float4> cleanFlow : register(t2);
Texture2D<float4> occConf   : register(t3);
StructuredBuffer<FrameDecision> decisionBuf : register(t4);

struct VSOut
{
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

VSOut VSMain(uint id : SV_VertexID)
{
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float4 PSMain(VSOut input) : SV_Target0
{
    float2 uv = ClampUv(input.uv, fullSize);
    uint flags = decisionBuf[0].flags;

    if ((flags & FD_STATIC_PANEL) != 0u || alpha >= 0.999)
    {
        return float4(SampleBicubicClamped(currColor, uv, fullSize), 1.0);
    }
    if (alpha <= 0.001)
    {
        return float4(SampleBicubicClamped(prevColor, uv, fullSize), 1.0);
    }
    if (IsOutsideActiveUv(uv, activeUvMin, activeUvMax))
    {
        return float4(0.0, 0.0, 0.0, 1.0);
    }
    if ((flags & FD_SCENE_CUT) != 0u)
    {
        return float4(SampleBicubicClamped(currColor, uv, fullSize), 1.0);
    }
    if ((flags & FD_GLOBAL_FADE) != 0u)
    {
        float3 pFade = SampleBicubicClamped(prevColor, uv, fullSize);
        float3 cFade = SampleBicubicClamped(currColor, uv, fullSize);
        return float4(lerp(pFade, cFade, alpha), 1.0);
    }

    float2 invFull = 1.0 / float2(fullSize);

    float4 flowC = cleanFlow.SampleLevel(sampLinear, ClampActiveUv(uv, coarseSize, activeUvMin, activeUvMax), 0);
    float4 occC  = occConf.SampleLevel(sampLinear, ClampActiveUv(uv, coarseSize, activeUvMin, activeUvMax), 0);

    // 1-step advected midpoint lookup along flow trajectory
    float2 uvAdvF = ClampActiveUv(uv - alpha * flowC.xy * invFull, coarseSize, activeUvMin, activeUvMax);
    float2 uvAdvB = ClampActiveUv(uv - (1.0 - alpha) * flowC.zw * invFull, coarseSize, activeUvMin, activeUvMax);

    float2 vfAdv = cleanFlow.SampleLevel(sampLinear, uvAdvF, 0).xy;
    float2 vbAdv = cleanFlow.SampleLevel(sampLinear, uvAdvB, 0).zw;

    float2 vf = lerp(flowC.xy, vfAdv, 0.5);
    float2 vb = lerp(flowC.zw, vbAdv, 0.5);

    // Exact static pixel bypass (zero blur on motionless dialogue backgrounds, text, or panels)
    if (max(dot(vf, vf), dot(vb, vb)) < 0.04)
    {
        float3 p0 = SampleBicubicClamped(prevColor, uv, fullSize);
        float3 c0 = SampleBicubicClamped(currColor, uv, fullSize);
        return float4(lerp(p0, c0, alpha), 1.0);
    }

    float2 uvPf = ClampActiveUv(uv - alpha * vf * invFull, fullSize, activeUvMin, activeUvMax);
    float2 uvCf = ClampActiveUv(uv + (1.0 - alpha) * vf * invFull, fullSize, activeUvMin, activeUvMax);
    float2 uvPb = ClampActiveUv(uv + alpha * vb * invFull, fullSize, activeUvMin, activeUvMax);
    float2 uvCb = ClampActiveUv(uv - (1.0 - alpha) * vb * invFull, fullSize, activeUvMin, activeUvMax);

    bool useBicubic = sharpBlend > 0.5;
    float3 pf = useBicubic ? SampleBicubicClamped(prevColor, uvPf, fullSize) : prevColor.SampleLevel(sampLinear, uvPf, 0).rgb;
    float3 cf = useBicubic ? SampleBicubicClamped(currColor, uvCf, fullSize) : currColor.SampleLevel(sampLinear, uvCf, 0).rgb;
    float3 pb = useBicubic ? SampleBicubicClamped(prevColor, uvPb, fullSize) : prevColor.SampleLevel(sampLinear, uvPb, 0).rgb;
    float3 cb = useBicubic ? SampleBicubicClamped(currColor, uvCb, fullSize) : currColor.SampleLevel(sampLinear, uvCb, 0).rgb;

    float errF = dot(abs(pf - cf), float3(0.3, 0.59, 0.11));
    float errB = dot(abs(pb - cb), float3(0.3, 0.59, 0.11));

    float occF = occConf.SampleLevel(sampLinear, uvAdvF, 0).x;
    float occB = occConf.SampleLevel(sampLinear, uvAdvB, 0).y;

    float wf = max(occF, 0.05) * exp(-errF * 18.0);
    float wb = max(occB, 0.05) * exp(-errB * 18.0);

    // Asymmetric disocclusion blending eliminates foreground ghosting halos
    float tf = alpha;
    float tb = alpha;
    if (occF < 0.35 && occB > 0.65)
    {
        float bias = 0.65 * (occB - occF);
        tb = lerp(alpha, 1.0, bias);
    }
    else if (occB < 0.35 && occF > 0.65)
    {
        float bias = 0.65 * (occF - occB);
        tf = lerp(alpha, 0.0, bias);
    }

    float3 candF = lerp(pf, cf, tf);
    float3 candB = lerp(pb, cb, tb);
    float3 warped = (candF * wf + candB * wb) / max(wf + wb, 1e-5);

    float warpSafety = occC.w;
    if (warpSafety < 0.999)
    {
        float3 pRaw = SampleBicubicClamped(prevColor, uv, fullSize);
        float3 cRaw = SampleBicubicClamped(currColor, uv, fullSize);
        float3 linearMix = lerp(pRaw, cRaw, alpha);
        float3 sharpSnap = (alpha < 0.5) ? pRaw : cRaw;
        float ghostDiff = length(pRaw - cRaw);
        // High-contrast silhouette differences (fast hands/arms/props) create ghosting
        // when alpha-blended naively; smoothly prefer single-frame selection instead.
        float snapWeight = (contentMode == MODE_COMIC)
            ? 1.0
            : smoothstep(0.07, 0.22, ghostDiff);
        float3 fallback = lerp(linearMix, sharpSnap, snapWeight);
        warped = lerp(fallback, warped, saturate(warpSafety * 1.35));
    }

    return float4(warped, 1.0);
}
