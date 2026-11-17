#include "InterpCommon.hlsli"

Texture2D<float4> texSrc0 : register(t0);
Texture2D<float4> texSrc1 : register(t1);

RWTexture2D<float4> outPyr : register(u0);
RWStructuredBuffer<FrameDecision> outDecision : register(u1);

float LoadLumaClamped(int2 p, int2 maxCoord)
{
    int2 cp = clamp(p, int2(0, 0), maxCoord);
    return CalcLuma(texSrc0.Load(int3(cp, 0)).rgb);
}

[numthreads(8, 8, 1)]
void CSPreprocessL0(uint3 dtid : SV_DispatchThreadID)
{
    if (dtid.x >= srcSize.x || dtid.y >= srcSize.y)
        return;

    int2 p = int2(dtid.xy);
    int2 maxCoord = int2(srcSize) - 1;

    float tl = LoadLumaClamped(p + int2(-1, -1), maxCoord);
    float tc = LoadLumaClamped(p + int2( 0, -1), maxCoord);
    float tr = LoadLumaClamped(p + int2( 1, -1), maxCoord);
    float ml = LoadLumaClamped(p + int2(-1,  0), maxCoord);
    float mc = LoadLumaClamped(p,                maxCoord);
    float mr = LoadLumaClamped(p + int2( 1,  0), maxCoord);
    float bl = LoadLumaClamped(p + int2(-1,  1), maxCoord);
    float bc = LoadLumaClamped(p + int2( 0,  1), maxCoord);
    float br = LoadLumaClamped(p + int2( 1,  1), maxCoord);

    float gx = ((tr + 2.0 * mr + br) - (tl + 2.0 * ml + bl)) * 0.25;
    float gy = ((bl + 2.0 * bc + br) - (tl + 2.0 * tc + tr)) * 0.25;
    float gmag = sqrt(gx * gx + gy * gy);

    outPyr[dtid.xy] = float4(mc, gx, gy, gmag);
}

[numthreads(8, 8, 1)]
void CSDownsample2x(uint3 dtid : SV_DispatchThreadID)
{
    if (dtid.x >= srcSize.x || dtid.y >= srcSize.y)
        return;

    float2 invCoarse = 1.0 / float2(max(coarseSize, uint2(1, 1)));
    float2 basePx = float2(dtid.xy) * 2.0 + 1.0;

    float4 s00 = texSrc0.SampleLevel(sampLinear, ClampUv((basePx + float2(-0.5, -0.5)) * invCoarse, coarseSize), 0);
    float4 s10 = texSrc0.SampleLevel(sampLinear, ClampUv((basePx + float2( 0.5, -0.5)) * invCoarse, coarseSize), 0);
    float4 s01 = texSrc0.SampleLevel(sampLinear, ClampUv((basePx + float2(-0.5,  0.5)) * invCoarse, coarseSize), 0);
    float4 s11 = texSrc0.SampleLevel(sampLinear, ClampUv((basePx + float2( 0.5,  0.5)) * invCoarse, coarseSize), 0);

    float4 avg = (s00 + s10 + s01 + s11) * 0.25;
    float maxG = max(max(s00.a, s10.a), max(s01.a, s11.a));
    // Preserve thin ink lines / text edges in channel A across coarser pyramid levels
    avg.a = lerp(avg.a, maxG, 0.35);

    outPyr[dtid.xy] = avg;
}

groupshared float gsMad[256];
groupshared float gsChanged[256];
groupshared float gsGradDiff[256];
groupshared float gsBotMad[256];
groupshared float gsBotCnt[256];
groupshared float gsNonBotMad[256];
groupshared float gsNonBotChg[256];
groupshared float gsNonBotCnt[256];
groupshared float gsYa[256];
groupshared float gsYb[256];
groupshared float gsYa2[256];
groupshared float gsYb2[256];
groupshared float gsYaYb[256];

[numthreads(16, 16, 1)]
void CSFrameCompare(uint3 gtid : SV_GroupThreadID, uint gidx : SV_GroupIndex)
{
    float madSum = 0.0;
    float chgSum = 0.0;
    float gradSum = 0.0;
    float botMadSum = 0.0;
    float botCnt = 0.0;
    float nonBotMadSum = 0.0;
    float nonBotChgSum = 0.0;
    float nonBotCnt = 0.0;
    float yaSum = 0.0;
    float ybSum = 0.0;
    float ya2Sum = 0.0;
    float yb2Sum = 0.0;
    float yaybSum = 0.0;

    // Single workgroup samples a stratified 64x64 grid (4096 points) strictly inside active video UVs
    // so black pillarboxes/letterboxes (e.g. 9:16 in 16:10 fullscreen) never dilute change/cut detection.
    float2 uvSpan = max(activeUvMax - activeUvMin, float2(1e-3, 1e-3));
    [unroll]
    for (uint sy = 0; sy < 4; ++sy)
    {
        [unroll]
        for (uint sx = 0; sx < 4; ++sx)
        {
            uint gx = gtid.x * 4u + sx;
            uint gy = gtid.y * 4u + sy;
            float2 relUv = (float2(gx, gy) + 0.5) / 64.0;
            float2 uv = activeUvMin + relUv * uvSpan;

            float4 a = texSrc0.SampleLevel(sampLinear, ClampActiveUv(uv, srcSize, activeUvMin, activeUvMax), 0);
            float4 b = texSrc1.SampleLevel(sampLinear, ClampActiveUv(uv, srcSize, activeUvMin, activeUvMax), 0);

            float dy = abs(a.r - b.r);
            float dg = abs(a.a - b.a);
            float chg = dy > 0.032 ? 1.0 : 0.0;

            madSum += dy;
            chgSum += chg;
            gradSum += dg;

            if (relUv.y >= subBandStartY && relUv.y <= subBandEndY)
            {
                botMadSum += dy;
                botCnt += 1.0;
            }
            if (relUv.y < subBandStartY)
            {
                nonBotMadSum += dy;
                nonBotChgSum += chg;
                nonBotCnt += 1.0;
            }

            yaSum += a.r;
            ybSum += b.r;
            ya2Sum += a.r * a.r;
            yb2Sum += b.r * b.r;
            yaybSum += a.r * b.r;
        }
    }

    gsMad[gidx] = madSum;
    gsChanged[gidx] = chgSum;
    gsGradDiff[gidx] = gradSum;
    gsBotMad[gidx] = botMadSum;
    gsBotCnt[gidx] = botCnt;
    gsNonBotMad[gidx] = nonBotMadSum;
    gsNonBotChg[gidx] = nonBotChgSum;
    gsNonBotCnt[gidx] = nonBotCnt;
    gsYa[gidx] = yaSum;
    gsYb[gidx] = ybSum;
    gsYa2[gidx] = ya2Sum;
    gsYb2[gidx] = yb2Sum;
    gsYaYb[gidx] = yaybSum;

    GroupMemoryBarrierWithGroupSync();

    [unroll]
    for (uint stride = 128u; stride > 0u; stride >>= 1u)
    {
        if (gidx < stride)
        {
            uint o = gidx + stride;
            gsMad[gidx] += gsMad[o];
            gsChanged[gidx] += gsChanged[o];
            gsGradDiff[gidx] += gsGradDiff[o];
            gsBotMad[gidx] += gsBotMad[o];
            gsBotCnt[gidx] += gsBotCnt[o];
            gsNonBotMad[gidx] += gsNonBotMad[o];
            gsNonBotChg[gidx] += gsNonBotChg[o];
            gsNonBotCnt[gidx] += gsNonBotCnt[o];
            gsYa[gidx] += gsYa[o];
            gsYb[gidx] += gsYb[o];
            gsYa2[gidx] += gsYa2[o];
            gsYb2[gidx] += gsYb2[o];
            gsYaYb[gidx] += gsYaYb[o];
        }
        GroupMemoryBarrierWithGroupSync();
    }

    if (gidx == 0u)
    {
        const float n = 4096.0;
        float madMean = gsMad[0] / n;
        float changedFrac = gsChanged[0] / n;
        float gradDiffMean = gsGradDiff[0] / n;
        float bottomMad = gsBotCnt[0] > 0.0 ? (gsBotMad[0] / gsBotCnt[0]) : 0.0;
        float nbMad = gsNonBotCnt[0] > 0.0 ? (gsNonBotMad[0] / gsNonBotCnt[0]) : madMean;
        float nbChg = gsNonBotCnt[0] > 0.0 ? (gsNonBotChg[0] / gsNonBotCnt[0]) : changedFrac;

        float meanA = gsYa[0] / n;
        float meanB = gsYb[0] / n;
        float lumaMeanDiff = abs(meanA - meanB);

        float varA = max(0.0, (gsYa2[0] / n) - meanA * meanA);
        float varB = max(0.0, (gsYb2[0] / n) - meanB * meanB);
        float cov  = (gsYaYb[0] / n) - meanA * meanB;
        float corr = (varA > 0.0002 && varB > 0.0002) ? (cov / sqrt(varA * varB)) : 1.0;

        bool isDup = (madMean <= dupMadMax) && (changedFrac <= dupChangedMax);
        bool isStatic = (contentMode == MODE_COMIC)
            ? ((nbMad <= staticMadMax && nbChg <= staticChangedMax) || (madMean <= staticMadMax && changedFrac <= staticChangedMax))
            : (madMean <= staticMadMax && changedFrac <= staticChangedMax);

        bool isFade = !isDup && !isStatic
            && ((lumaMeanDiff >= 0.075 && changedFrac >= 0.62 && corr >= 0.68)
                || (lumaMeanDiff >= 0.18 && changedFrac >= 0.58 && corr >= 0.45));
        bool isCut = !isDup && !isStatic && !isFade &&
            ((madMean >= cutMadMin && changedFrac >= cutChangedMin && corr < 0.68)
            || gradDiffMean >= cutGradMin
            || (corr < 0.22 && changedFrac >= 0.55));

        uint flags = 0u;
        if (isStatic) flags |= FD_STATIC_PANEL;
        if (isCut)    flags |= FD_SCENE_CUT;
        if (isDup)    flags |= FD_DUPLICATE;
        if (isFade)   flags |= FD_GLOBAL_FADE;

        FrameDecision fd;
        fd.madMean = madMean;
        fd.changedFrac = changedFrac;
        fd.gradDiffMean = gradDiffMean;
        fd.bottomMad = bottomMad;
        fd.lumaMeanDiff = lumaMeanDiff;
        fd.nonBottomMad = nbMad;
        fd.flags = flags;
        fd.pad = 0u;
        outDecision[0] = fd;
    }
}
