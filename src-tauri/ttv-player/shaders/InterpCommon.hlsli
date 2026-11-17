#ifndef INTERP_COMMON_HLSLI
#define INTERP_COMMON_HLSLI

static const uint FD_STATIC_PANEL = 1u << 0;
static const uint FD_SCENE_CUT    = 1u << 1;
static const uint FD_DUPLICATE    = 1u << 2;
static const uint FD_GLOBAL_FADE  = 1u << 3;

static const uint MODE_DRAMA = 0u;
static const uint MODE_COMIC = 1u;
static const uint MODE_ANIME = 2u;

struct FrameDecision
{
    float madMean;
    float changedFrac;
    float gradDiffMean;
    float bottomMad;
    float lumaMeanDiff;
    float nonBottomMad;
    uint  flags;
    uint  pad;
};

cbuffer InterpCB : register(b0)
{
    uint2  srcSize;
    uint2  fullSize;
    uint2  coarseSize;
    float  alpha;
    uint   contentMode;

    float  flatZeroBias;
    float  gradWeight;
    float  edgeSharpness;
    float  subLockWeight;

    float  dupMadMax;
    float  dupChangedMax;
    float  staticMadMax;
    float  staticChangedMax;

    float  cutMadMin;
    float  cutChangedMin;
    float  cutGradMin;
    float  maxSafeFlowPx;

    float  levelScale;
    float  smoothPrior;
    float  occStrictness;
    float  sharpBlend;

    float2 activeUvMin;
    float2 activeUvMax;

    float  subBandStartY;
    float  subBandEndY;
    float  portraitFactor;
    float  aspectPad;
};

SamplerState sampLinear : register(s0);
SamplerState sampPoint  : register(s1);

float CalcLuma(float3 rgb)
{
    return dot(rgb, float3(0.2126, 0.7152, 0.0722));
}

float2 ClampUv(float2 uv, uint2 size)
{
    float2 halfTexel = 0.5 / float2(max(size, uint2(1, 1)));
    return clamp(uv, halfTexel, 1.0 - halfTexel);
}

float2 ClampActiveUv(float2 uv, uint2 size, float2 uvMin, float2 uvMax)
{
    float2 halfTexel = 0.5 / float2(max(size, uint2(1, 1)));
    float2 lo = min(uvMin + halfTexel, (uvMin + uvMax) * 0.5);
    float2 hi = max(uvMax - halfTexel, lo);
    return clamp(uv, lo, hi);
}

bool IsOutsideActiveUv(float2 uv, float2 uvMin, float2 uvMax)
{
    return (uv.x < uvMin.x || uv.y < uvMin.y || uv.x > uvMax.x || uv.y > uvMax.y);
}

// 5-bilinear-tap Catmull-Rom reconstruction clamped to the immediate 2x2 neighborhood
// to keep anime ink lines and burnt-in subtitles free of ringing halos.
float3 SampleBicubicClamped(Texture2D<float4> tex, float2 uv, uint2 size)
{
    float2 fSize = float2(size);
    float2 invSize = 1.0 / fSize;
    float2 p = uv * fSize - 0.5;
    float2 fp = floor(p);
    float2 f = p - fp;
    float2 f2 = f * f;
    float2 f3 = f2 * f;

    float2 w0 = -0.5 * f3 + f2 - 0.5 * f;
    float2 w1 =  1.5 * f3 - 2.5 * f2 + 1.0;
    float2 w2 = -1.5 * f3 + 2.0 * f2 + 0.5 * f;
    float2 w3 =  0.5 * f3 - 0.5 * f2;

    float2 w12 = w1 + w2;
    float2 offset12 = w2 / max(w12, 1e-5);

    float2 tc0  = ClampUv((fp - 0.5) * invSize, size);
    float2 tc3  = ClampUv((fp + 2.5) * invSize, size);
    float2 tc12 = ClampUv((fp + 0.5 + offset12) * invSize, size);

    float3 sC = tex.SampleLevel(sampLinear, tc12, 0).rgb;
    float3 sN = tex.SampleLevel(sampLinear, float2(tc12.x, tc0.y), 0).rgb;
    float3 sS = tex.SampleLevel(sampLinear, float2(tc12.x, tc3.y), 0).rgb;
    float3 sW = tex.SampleLevel(sampLinear, float2(tc0.x, tc12.y), 0).rgb;
    float3 sE = tex.SampleLevel(sampLinear, float2(tc3.x, tc12.y), 0).rgb;

    float wc = w12.x * w12.y;
    float wn = w12.x * w0.y;
    float ws = w12.x * w3.y;
    float ww = w0.x * w12.y;
    float we = w3.x * w12.y;
    float totalW = wc + wn + ws + ww + we;

    float3 res = (sC * wc + sN * wn + sS * ws + sW * ww + sE * we) / max(totalW, 1e-5);

    float2 p00 = ClampUv((fp + 0.5) * invSize, size);
    float2 p10 = ClampUv((fp + float2(1.5, 0.5)) * invSize, size);
    float2 p01 = ClampUv((fp + float2(0.5, 1.5)) * invSize, size);
    float2 p11 = ClampUv((fp + 1.5) * invSize, size);
    float3 c00 = tex.SampleLevel(sampPoint, p00, 0).rgb;
    float3 c10 = tex.SampleLevel(sampPoint, p10, 0).rgb;
    float3 c01 = tex.SampleLevel(sampPoint, p01, 0).rgb;
    float3 c11 = tex.SampleLevel(sampPoint, p11, 0).rgb;

    float3 nMin = min(min(c00, c10), min(c01, c11));
    float3 nMax = max(max(c00, c10), max(c01, c11));
    return clamp(res, nMin, nMax);
}

#endif
