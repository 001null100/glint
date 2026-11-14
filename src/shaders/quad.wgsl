// Instanced rounded rectangles: fills, gradients, borders, soft shadows /
// glows and frosted panels (which sample the blurred background).

struct G {
    screen: vec4<f32>,   // width, height, time, unused
};

@group(0) @binding(0) var<uniform> g: G;
@group(0) @binding(1) var frost_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct Inst {
    @location(0) rect: vec4<f32>,          // x, y, w, h (pixels)
    @location(1) color: vec4<f32>,
    @location(2) color2: vec4<f32>,        // gradient end
    @location(3) border_color: vec4<f32>,
    @location(4) radii: vec4<f32>,         // tl, tr, br, bl
    @location(5) params: vec4<f32>,        // border width, softness, frost, gradient dir (0 none, 1 vertical, 2 horizontal)
    @location(6) clip: vec4<f32>,          // x0, y0, x1, y1
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half: vec2<f32>,
    @location(2) @interpolate(flat) color: vec4<f32>,
    @location(3) @interpolate(flat) color2: vec4<f32>,
    @location(4) @interpolate(flat) border_color: vec4<f32>,
    @location(5) @interpolate(flat) radii: vec4<f32>,
    @location(6) @interpolate(flat) params: vec4<f32>,
    @location(7) @interpolate(flat) clip: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let margin = inst.params.y * 1.5 + 1.0;
    let size = inst.rect.zw + 2.0 * margin;
    let px = inst.rect.xy - margin + corner * size;
    var o: VOut;
    o.pos = vec4<f32>(px.x / g.screen.x * 2.0 - 1.0, 1.0 - px.y / g.screen.y * 2.0, 0.0, 1.0);
    o.local = px - (inst.rect.xy + inst.rect.zw * 0.5);
    o.half = inst.rect.zw * 0.5;
    o.color = inst.color;
    o.color2 = inst.color2;
    o.border_color = inst.border_color;
    o.radii = inst.radii;
    o.params = inst.params;
    o.clip = inst.clip;
    return o;
}

fn sd_rbox(pt: vec2<f32>, b: vec2<f32>, r: vec4<f32>) -> f32 {
    // r = tl, tr, br, bl; y grows downwards
    let top = select(r.x, r.y, pt.x > 0.0);
    let bot = select(r.w, r.z, pt.x > 0.0);
    let rad = min(select(top, bot, pt.y > 0.0), min(b.x, b.y));
    let q = abs(pt) - b + rad;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - rad;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let frag = in.pos.xy;
    if frag.x < in.clip.x || frag.y < in.clip.y || frag.x >= in.clip.z || frag.y >= in.clip.w {
        discard;
    }
    let d = sd_rbox(in.local, in.half, in.radii);
    let soft = in.params.y;
    var cov: f32;
    if soft > 0.0 {
        // gaussian-ish falloff for shadows and glows
        let x = clamp(0.5 - d / (2.0 * soft), 0.0, 1.0);
        cov = x * x * (3.0 - 2.0 * x);
    } else {
        cov = clamp(0.5 - d, 0.0, 1.0);
    }
    if cov <= 0.0 {
        discard;
    }

    var fill = in.color;
    let gd = in.params.w;
    if gd > 0.5 {
        var t: f32;
        if gd < 1.5 {
            t = in.local.y / (2.0 * in.half.y) + 0.5;
        } else {
            t = in.local.x / (2.0 * in.half.x) + 0.5;
        }
        fill = mix(in.color, in.color2, clamp(t, 0.0, 1.0));
    }

    if in.params.z > 0.0 {
        // frosted: blurred backdrop tinted by the fill colour
        let bgc = textureSampleLevel(frost_tex, samp, frag / g.screen.xy, 0.0).rgb;
        fill = vec4<f32>(mix(bgc, fill.rgb, fill.a), mix(fill.a, 1.0, in.params.z));
    }

    let bw = in.params.x;
    if bw > 0.0 {
        let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
        let bc = in.border_color;
        // border over fill
        let ba = bc.a * (1.0 - inner);
        let out_a = ba + fill.a * (1.0 - ba);
        let rgb = (bc.rgb * ba + fill.rgb * fill.a * (1.0 - ba)) / max(out_a, 1e-5);
        fill = vec4<f32>(rgb, out_a);
    }

    return vec4<f32>(fill.rgb, fill.a * cov);
}
