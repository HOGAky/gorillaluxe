//! Runtime HLSL compilation via d3dcompiler_47.dll (present on all
//! Windows 10/11 hosts and CI runners). Root signatures are embedded
//! in the shader blobs via the [RootSignature] attribute.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};

use super::{hr, GResult};

const OPT_LEVEL3: u32 = 0x8000; // D3DCOMPILE_OPTIMIZATION_LEVEL3

/// Compile `src` for `entry`/`target` (e.g. "CSMain"/"cs_5_1").
/// Returns the shader bytecode incl. any embedded root signature.
pub fn compile(src: &str, entry: &'static str, target: &'static str) -> GResult<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut msgs: Option<ID3DBlob> = None;
    unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            windows::core::s!("gorillaluxe.hlsl"),
            None,
            None::<&ID3DInclude>,
            windows::core::PCSTR(entry.as_ptr()),
            windows::core::PCSTR(target.as_ptr()),
            OPT_LEVEL3,
            0,
            &mut code,
            Some(&mut msgs),
        )
        .map_err(|e| {
            let detail = msgs
                .as_ref()
                .map(|b| {
                    let n = b.GetBufferSize();
                    let p = b.GetBufferPointer() as *const u8;
                    String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).to_string()
                })
                .unwrap_or_default();
            format!("{}: {}", hr("D3DCompile", e), detail.trim_end())
        })?;
    }
    let blob = code.ok_or_else(|| "D3DCompile returned no code".to_string())?;
    unsafe {
        Ok(std::slice::from_raw_parts(
            blob.GetBufferPointer() as *const u8,
            blob.GetBufferSize(),
        )
        .to_vec())
    }
}

/// HLSL entry point names.
pub const CAS_ENTRY: &str = "CSMain\0";
pub const PATTERN_ENTRY: &str = "CSMain\0";
pub const UI_VS_ENTRY: &str = "VSMain\0";
pub const UI_PS_ENTRY: &str = "PSMain\0";

/// Guest framebuffer pattern generator — mirrors `pattern::pattern_pixel`
/// exactly (integer math), so the CPU reference image matches 1:1.
pub const PATTERN_SRC: &str = r#"
cbuffer Params : register(b0) { uint W; uint H; uint Pad0; uint Pad1; }
RWTexture2D<float4> Dst : register(u0);

[RootSignature("DescriptorTable(UAV(u0)),RootConstants(num32BitConstants=4,b0)")]
[numthreads(8,8,1)]
void CSMain(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= W || tid.y >= H) return;
    uint r = (tid.x * 255u) / max(W - 1u, 1u);
    uint g = (tid.y * 255u) / max(H - 1u, 1u);
    uint b = (((tid.x / 32u) + (tid.y / 32u)) & 1u) * 255u;
    uint m = ((tid.x + tid.y) & 1u) * 48u;
    r = min(r + m, 255u);
    g = min(g + m, 255u);
    Dst[tid.xy] = float4(r / 255.0f, g / 255.0f, b / 255.0f, 1.0f);
}
"#;

/// AMD FidelityFX CAS — single-pass 5-tap contrast-adaptive sharpen with
/// anti-ringing clamps, R8G8B8A8_UNORM lossless (spec §2.2.3).
/// `Sharpness` maps onto the CAS peak: 0 = softest, 1 = sharpest.
pub const CAS_SRC: &str = r#"
cbuffer Params : register(b0) {
    uint SrcW; uint SrcH; uint DstW; uint DstH;
    float Sharpness; float Pad0; float Pad1; float Pad2;
}
Texture2D<float4> SrcTex : register(t0);
RWTexture2D<float4> DstTex : register(u0);

float3 Load(int2 p) {
    int2 c = clamp(p, int2(0, 0), int2(int(SrcW) - 1, int(SrcH) - 1));
    return SrcTex[c].rgb;
}
float3 Min3(float3 a, float3 b, float3 c) { return min(a, min(b, c)); }
float3 Max3(float3 a, float3 b, float3 c) { return max(a, max(b, c)); }

[RootSignature("DescriptorTable(SRV(t0),UAV(u0)),RootConstants(num32BitConstants=8,b0)")]
[numthreads(8,8,1)]
void CSMain(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= DstW || tid.y >= DstH) return;
    float2 sp = (float2(tid.xy) + 0.5f) * (float2(SrcW, SrcH) / float2(DstW, DstH));
    int2 ic = int2(sp);
    float3 e = Load(ic);
    float3 b = Load(ic + int2(0, -1));
    float3 d = Load(ic + int2(-1, 0));
    float3 f = Load(ic + int2(1, 0));
    float3 h = Load(ic + int2(0, 1));
    float3 mn = min(Min3(b, d, f), min(h, e));
    float3 mx = max(Max3(b, d, f), max(h, e));
    mx = max(mx, 1e-5f);
    float peak = -1.0f / lerp(8.0f, 5.0f, Sharpness);
    float3 w = peak * sqrt(saturate(min(mn, 2.0f - mx) / mx));
    float3 col = (e + (b + d + f + h) * w) / (1.0f + 4.0f * w);
    DstTex[tid.xy] = float4(saturate(col), 1.0f);
}
"#;

/// Fullscreen blit: CAS output -> swapchain back buffer (linear copy on GPU).
pub const BLIT_VS_SRC: &str = r#"
struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VsOut VSMain(uint vid : SV_VertexID) {
    float2 p = float2((vid << 1) & 2, vid & 2);
    VsOut o;
    o.pos = float4(p.x * 2.0f - 1.0f, 1.0f - p.y * 2.0f, 0.0f, 1.0f);
    o.uv = p;
    return o;
}
"#;

pub const BLIT_PS_SRC: &str = r#"
Texture2D<float4> SrcTex : register(t0);
SamplerState Smp : register(s0);
struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
[RootSignature("DescriptorTable(SRV(t0)),StaticSampler(s0,filter=FILTER_MIN_MAG_LINEAR_MIP_POINT,addressU=TEXTURE_ADDRESS_CLAMP,addressV=TEXTURE_ADDRESS_CLAMP)")]
float4 PSMain(VsOut i) : SV_Target {
    return SrcTex.Sample(Smp, i.uv);
}
"#;

/// UI quad renderer: SDF rounded rects + R8 font-atlas glyphs.
/// Instances: pos(2) size(2) uv(4) color(4) radius(1) pad(3) = 64 B.
pub const UI_VS_SRC: &str = r#"
cbuffer Screen : register(b0) { float2 ScreenSize; float2 ScrPad; }
struct Inst {
    float2 pos; float2 size; float4 uv; float4 color;
    float radius; float3 pad;
};
StructuredBuffer<Inst> Insts : register(t0);
struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
    float4 color : COLOR0;
    float2 local : TEXCOORD1;
    float2 half_ : TEXCOORD2;
    nointerpolation float radius : TEXCOORD3;
    nointerpolation float2 uvSize : TEXCOORD4;
};
VsOut VSMain(uint vid : SV_VertexID, uint iid : SV_InstanceID) {
    Inst I = Insts[iid];
    float qx = (vid == 1u || vid == 2u || vid == 4u) ? 1.0f : 0.0f;
    float qy = (vid == 2u || vid == 4u || vid == 5u) ? 1.0f : 0.0f;
    float2 q = float2(qx, qy);
    float2 px = I.pos + q * I.size;
    VsOut o;
    o.pos = float4(
        px.x / ScreenSize.x * 2.0f - 1.0f,
        1.0f - px.y / ScreenSize.y * 2.0f,
        0.0f,
        1.0f
    );
    o.uv = I.uv.xy + q * I.uv.zw;
    o.color = I.color;
    o.local = q * I.size;
    o.half_ = I.size * 0.5f;
    o.radius = I.radius;
    o.uvSize = I.uv.zw;
    return o;
}
"#;

pub const UI_PS_SRC: &str = r#"
Texture2D<float> Atlas : register(t1);
SamplerState Smp : register(s0);
struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
    float4 color : COLOR0;
    float2 local : TEXCOORD1;
    float2 half_ : TEXCOORD2;
    nointerpolation float radius : TEXCOORD3;
    nointerpolation float2 uvSize : TEXCOORD4;
};
[RootSignature("DescriptorTable(SRV(t0),SRV(t1)),RootConstants(num32BitConstants=2,b0),StaticSampler(s0,filter=FILTER_MIN_MAG_LINEAR_MIP_POINT,addressU=TEXTURE_ADDRESS_CLAMP,addressV=TEXTURE_ADDRESS_CLAMP)")]
float4 PSMain(VsOut i) : SV_Target {
    float a = i.color.a;
    if (i.uvSize.x > 0.0f || i.uvSize.y > 0.0f) {
        a *= Atlas.Sample(Smp, i.uv).r;
    } else {
        float2 q = abs(i.local - i.half_) - (i.half_ - i.radius);
        float d = length(max(q, 0.0f)) - i.radius;
        a *= 1.0f - saturate(d + 0.5f);
    }
    // Premultiplied output — SrcBlend is ONE: without the *a term
    // every glyph/rect quad adds full rgb even where alpha == 0,
    // which renders text as solid blocks.
    return float4(i.color.rgb * a, a);
}
"#;

/// Luxe Dark ambient backdrop for the live compositor (spec §3.6):
/// deep obsidian, subtle dark grid lines, soft golden center glow.
/// Same root signature as PATTERN_SRC.
pub const AMBIENT_SRC: &str = r#"
cbuffer Params : register(b0) { uint W; uint H; uint Pad0; uint Pad1; }
RWTexture2D<float4> Dst : register(u0);

[RootSignature("DescriptorTable(UAV(u0)),RootConstants(num32BitConstants=4,b0)")]
[numthreads(8,8,1)]
void CSMain(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= W || tid.y >= H) return;
    float2 c = (float2(tid.xy) + 0.5f) / float2(W, H) - float2(0.5f, 0.42f);
    float d = length(c * float2(1.15f, 1.0f));
    float3 base = lerp(
        float3(22.0f, 24.0f, 33.0f) / 255.0f,
        float3(13.0f, 14.0f, 18.0f) / 255.0f,
        saturate(d * 1.6f));
    float grid = ((tid.x % 64u) == 0u || (tid.y % 64u) == 0u) ? (5.0f / 255.0f) : 0.0f;
    float glow = exp(-d * d * 9.0f) * 0.16f;
    float3 col = base + grid + float3(0xF5, 0xA6, 0x23) / 255.0f * glow;
    Dst[tid.xy] = float4(col, 1.0f);
}
"#;
