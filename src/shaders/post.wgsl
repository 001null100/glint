// Fullscreen post passes: blur (for frosted panels) and the final blit of
// the background to the screen with vignette and dithering.

struct P {
    // blur: xy = sample step in uv, z = mode (0 box downsample, 1 gaussian)
    // blit: xy = screen size, z = time, w = vignette strength
    a: vec4<f32>,
};

@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    let x = f32(i32(i & 1u) * 4 - 1);
    let y = f32(i32(i >> 1u) * 4 - 1);
    var o: VOut;
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    o.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return o;
}

@fragment
fn fs_blur(in: VOut) -> @location(0) vec4<f32> {
    let d = p.a.xy;
    if p.a.z < 0.5 {
        // 4-tap box: each bilinear tap averages a 2x2 block
        let o = d;
        var c = textureSampleLevel(tex, samp, in.uv + vec2<f32>(-o.x, -o.y), 0.0).rgb;
        c += textureSampleLevel(tex, samp, in.uv + vec2<f32>(o.x, -o.y), 0.0).rgb;
        c += textureSampleLevel(tex, samp, in.uv + vec2<f32>(-o.x, o.y), 0.0).rgb;
        c += textureSampleLevel(tex, samp, in.uv + vec2<f32>(o.x, o.y), 0.0).rgb;
        return vec4<f32>(c * 0.25, 1.0);
    }
    // 9-tap gaussian using 5 bilinear fetches
    var c = textureSampleLevel(tex, samp, in.uv, 0.0).rgb * 0.2270270270;
    c += textureSampleLevel(tex, samp, in.uv + d * 1.3846153846, 0.0).rgb * 0.3162162162;
    c += textureSampleLevel(tex, samp, in.uv - d * 1.3846153846, 0.0).rgb * 0.3162162162;
    c += textureSampleLevel(tex, samp, in.uv + d * 3.2307692308, 0.0).rgb * 0.0702702703;
    c += textureSampleLevel(tex, samp, in.uv - d * 3.2307692308, 0.0).rgb * 0.0702702703;
    return vec4<f32>(c, 1.0);
}

fn ign(px: vec2<f32>) -> f32 {
    // interleaved gradient noise
    return fract(52.9829189 * fract(dot(px, vec2<f32>(0.06711056, 0.00583715))));
}

@fragment
fn fs_blit(in: VOut) -> @location(0) vec4<f32> {
    var c = textureSampleLevel(tex, samp, in.uv, 0.0).rgb;
    let q = in.uv - 0.5;
    let vig = 1.0 - p.a.w * smoothstep(0.25, 0.95, dot(q, q) * 2.0);
    c *= vig;
    // dither to hide banding in dark gradients
    let n = ign(in.pos.xy + fract(p.a.z) * 61.0) - 0.5;
    c += vec3<f32>(n / 255.0);
    return vec4<f32>(c, 1.0);
}
