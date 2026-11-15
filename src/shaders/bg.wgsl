// Animated backgrounds. One fragment shader holds every scene; two scene
// slots (a, b) are blended by `fade` so themes can cross-fade.
//
// Scene coordinates: uv.y runs -0.5 (bottom) .. 0.5 (top), uv.x is aspect
// corrected. Every scene returns `base + intensity * light`.

struct SceneP {
    a: vec4<f32>,    // intensity, time, scale, kind
    k: vec4<f32>,    // four scene knobs, 0..1
    c0: vec4<f32>,
    c1: vec4<f32>,
    c2: vec4<f32>,
    c3: vec4<f32>,
    base: vec4<f32>,
};

struct U {
    res: vec4<f32>,  // width, height, fade, unused
    // typing pulses: x, y (0..1 of the window, y down), age in seconds, strength
    p: array<vec4<f32>, 4>,
    sa: SceneP,
    sb: SceneP,
};

@group(0) @binding(0) var<uniform> u: U;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(i & 1u) * 4 - 1);
    let y = f32(i32(i >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

// ---------------------------------------------------------------- noise

fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn hash22(p: vec2<f32>) -> vec2<f32> {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * vec3<f32>(0.1031, 0.1030, 0.0973));
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.xx + p3.yz) * p3.zy);
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let w = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

fn fbm(p0: vec2<f32>, oct: i32) -> f32 {
    var p = p0;
    var amp = 0.5;
    var s = 0.0;
    for (var i = 0; i < oct; i++) {
        s += amp * vnoise(p);
        p = vec2<f32>(1.6 * p.x - 1.2 * p.y, 1.2 * p.x + 1.6 * p.y) + 7.3;
        amp *= 0.5;
    }
    return s;
}

// A sparse layer of point stars. `cells` = grid cells per unit of uv.
fn stars(uv: vec2<f32>, cells: f32, t: f32, seed: f32, twinkle: f32) -> f32 {
    let g = uv * cells;
    let id = floor(g);
    let f = fract(g) - 0.5;
    let r = hash22(id + seed);
    let pos = (r - 0.5) * 0.7;
    let d = length(f - pos);
    let m = hash21(id + seed + 17.3);
    let mag = m * m * m * m;
    let tw = 1.0 - twinkle * (0.5 + 0.5 * sin(t * (0.7 + r.x * 2.5) + r.y * 6.283));
    let core = exp(-d * d * 600.0);
    let halo = exp(-d * d * 90.0) * 0.3 * mag;
    return (0.1 + 0.9 * mag) * tw * (core + halo);
}

// ---------------------------------------------------------------- scenes

fn scene_plain(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let p1 = vec2<f32>(0.45 * sin(t * 0.05), 0.22 * cos(t * 0.041));
    let p2 = vec2<f32>(0.5 * cos(t * 0.037 + 2.0), 0.25 * sin(t * 0.046 + 1.0));
    let g1 = exp(-dot(uv - p1, uv - p1) * 3.0 / S.a.z);
    let g2 = exp(-dot(uv - p2, uv - p2) * 3.0 / S.a.z);
    let light = (S.c0.rgb * g1 + S.c1.rgb * g2) * 0.16 * (0.4 + 1.2 * S.k.x);
    return S.base.rgb + S.a.x * light;
}

fn scene_aurora(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let sc = S.a.z;
    let n = 1 + i32(S.k.x * 4.0 + 0.5);
    var light = vec3<f32>(0.0);
    var cover = 0.0;
    for (var i = 0; i < n; i++) {
        let fi = f32(i);
        let xx = uv.x * sc * (0.8 + 0.25 * fi) + fi * 4.1;
        let warp = fbm(vec2<f32>(xx * 0.9, t * 0.05 + fi * 2.0), 3);
        let base_y = (S.k.z - 0.5) - 0.1 + (warp - 0.47) * 0.6 - fi * 0.07;
        let d = uv.y - base_y;
        let ray = fbm(vec2<f32>(xx * 7.0 + warp * 4.0, t * 0.12 + fi + d * 0.6), 3);
        let rays = mix(0.8, smoothstep(0.22, 0.72, ray) * 1.7, S.k.y);
        let lower = smoothstep(-0.035, 0.0, d);
        let upper = exp(-max(d, 0.0) * (4.2 + 1.4 * fi));
        let shape = lower * upper;
        let tint = mix(S.c0.rgb, S.c1.rgb, smoothstep(0.0, 0.42, d + (warp - 0.5) * 0.15));
        light += tint * shape * rays * (0.55 / (1.0 + 0.55 * fi));
        cover = max(cover, shape);
    }
    // faint sky glow and stars
    let sky = S.c3.rgb * (0.35 + 0.5 * (1.0 - smoothstep(-0.5, 0.5, uv.y)));
    let st = (stars(uv + vec2<f32>(t * 0.002, 0.0), 46.0, t, 3.1, 0.7)
            + stars(uv + vec2<f32>(t * 0.001, 0.0) + 5.0, 90.0, t, 9.4, 0.7) * 0.6)
            * S.k.w * 1.2 * (1.0 - 0.7 * cover);
    light += S.c2.rgb * 0.05 * exp(-abs(uv.y - (S.k.z - 0.5)) * 2.5);
    return S.base.rgb + sky * 0.5 + S.a.x * (light + vec3<f32>(st));
}

fn scene_starfield(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let sc = S.a.z;
    let drift = vec2<f32>(t * 0.004, t * 0.0016);
    // nebula clouds
    let n1 = fbm(uv * 1.3 * sc + drift * 2.0 + 3.0, 5);
    let n2 = fbm(uv * 2.1 * sc - drift * 3.0 + 9.0, 4);
    var light = (S.c1.rgb * pow(n1, 3.0) * 1.3 + S.c2.rgb * pow(n2, 3.5) * 1.1) * S.k.z;
    // stars
    let dens = 0.55 + 0.9 * S.k.x;
    let tw = S.k.y;
    let l1 = stars(uv + drift, 26.0 * sc * dens, t, 1.7, tw);
    let l2 = stars(uv + drift * 0.6 + 11.0, 52.0 * sc * dens, t, 5.3, tw);
    let l3 = stars(uv + drift * 0.3 + 23.0, 96.0 * sc * dens, t, 8.9, tw);
    light += S.c0.rgb * l1 * 1.3 + mix(S.c0.rgb, S.c3.rgb, 0.6) * l2 * 0.9 + S.c0.rgb * l3 * 0.6;
    // occasional shooting star
    let period = 7.0;
    let kk = floor(t / period);
    let ph = fract(t / period);
    let r = hash22(vec2<f32>(kk, 4.7));
    let dir = normalize(vec2<f32>(-0.8 - r.y * 0.4, -0.35));
    let life = ph / 0.11;
    let head = vec2<f32>((r.x - 0.3) * 1.6, 0.15 + r.y * 0.3) + dir * life * 0.9;
    let rel = uv - head;
    let along = dot(rel, -dir);
    let perp = abs(rel.x * dir.y - rel.y * dir.x);
    let live = step(life, 1.0) * step(hash21(vec2<f32>(kk, 9.1)), S.k.w);
    let streak = (1.0 - smoothstep(0.0, 0.3, along)) * step(0.0, along) * exp(-perp * perp * 60000.0) * (1.0 - life) * live;
    light += S.c0.rgb * streak * 1.5;
    return S.base.rgb + S.a.x * light;
}

fn scene_nebula(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let p = uv * 1.5 * S.a.z;
    let oct = 3 + i32(S.k.w * 3.0);
    let q = vec2<f32>(fbm(p + vec2<f32>(0.0, t * 0.03), oct), fbm(p + vec2<f32>(5.2, 1.3) - t * 0.025, oct));
    let w = 1.0 + 3.0 * S.k.x;
    let r = vec2<f32>(
        fbm(p + w * q + vec2<f32>(1.7, 9.2) + t * 0.02, oct),
        fbm(p + w * q + vec2<f32>(8.3, 2.8) - t * 0.018, oct));
    let f = fbm(p + w * r, oct + 1);
    var c = mix(S.c3.rgb, S.c0.rgb, smoothstep(0.25, 0.8, f));
    c = mix(c, S.c1.rgb, smoothstep(0.35, 0.95, length(q)));
    c = mix(c, S.c2.rgb, smoothstep(0.45, 0.95, r.x) * 0.8);
    let dens = pow(clamp(f * 1.35, 0.0, 1.0), 1.5 + 3.0 * S.k.y);
    var light = c * dens * 0.9;
    let st = stars(uv + vec2<f32>(t * 0.0015, 0.0), 60.0, t, 2.2, 0.6) + stars(uv + 7.0, 110.0, t, 6.1, 0.6) * 0.6;
    light += vec3<f32>(st) * S.k.z * (1.0 - dens) * 1.3;
    return S.base.rgb + S.a.x * light;
}

fn scene_embers(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let sc = S.a.z;
    var light = vec3<f32>(0.0);
    // glow from below, flickering
    let fl = 0.75 + 0.5 * fbm(vec2<f32>(uv.x * 2.0, t * 0.35), 3);
    light += S.c1.rgb * exp(-(uv.y + 0.5) * 4.2) * 0.55 * fl * (0.3 + 1.4 * S.k.z);
    // slow smoke
    let smoke = fbm(vec2<f32>(uv.x * 1.6 * sc, uv.y * 1.6 * sc - t * 0.05) + 3.0, 4);
    light += S.c3.rgb * smoke * smoke * 0.9;
    // sparks
    let dens = 0.15 + 0.6 * S.k.x;
    for (var i = 0; i < 5; i++) {
        let fi = f32(i);
        let cells = (7.0 + 5.0 * fi) * sc;
        let rise = 0.025 + 0.018 * fi;
        let sway = sin(uv.y * 4.0 + t * 0.35 + fi * 1.7) * 0.05 * S.k.y;
        let g = vec2<f32>((uv.x + sway) * cells + fi * 13.7, (uv.y - t * rise) * cells * 0.75);
        let id = floor(g);
        let f = fract(g) - 0.5;
        let r = hash22(id + fi * 31.7);
        let on = step(r.x, dens);
        let pos = (hash22(id + 5.2 + fi) - 0.5) * 0.6;
        let d = length((f - pos) * vec2<f32>(1.0, 0.75));
        let size = (0.35 + S.k.w) * (1.4 - 0.18 * fi);
        let flick = 0.55 + 0.45 * sin(t * (2.5 + r.y * 5.0) + r.x * 40.0);
        let cool = (1.0 - smoothstep(-0.5, 0.6, uv.y));
        let spark = exp(-d * d * 520.0 / (size * size)) + 0.25 * exp(-d * d * 60.0 / (size * size));
        light += mix(S.c1.rgb, mix(S.c0.rgb, S.c2.rgb, r.y * 0.5), cool) * spark * flick * on * (0.25 + 0.75 * cool);
    }
    return S.base.rgb + S.a.x * light;
}

fn scene_grid(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let sc = S.a.z;
    let res_y = u.res.y;
    let hz = S.k.x - 0.5;
    var light = vec3<f32>(0.0);
    let below = hz - uv.y;
    if below > 0.0 {
        // perspective floor
        let kz = 0.22;
        let z = kz / (below + 0.004);
        let dz = kz / ((below + 0.004) * (below + 0.004));
        let gx = uv.x * z * 4.0 * sc;
        let gz = z * 4.0 * sc + t * 0.55;
        let wx = (z * 4.0 * sc + abs(uv.x) * dz * 4.0 * sc) / res_y;
        let wz = dz * 4.0 * sc / res_y;
        let lx = 1.0 - smoothstep(0.0, 0.03 + 1.5 * wx, abs(fract(gx + 0.5) - 0.5));
        let lz = 1.0 - smoothstep(0.0, 0.03 + 1.5 * wz, abs(fract(gz + 0.5) - 0.5));
        // fade out detail that would alias near the horizon
        let fx = 1.0 - smoothstep(0.15, 0.5, wx);
        let fz = 1.0 - smoothstep(0.15, 0.5, wz);
        let dist = exp(-z * 0.18);
        light += S.c0.rgb * max(lx * fx, lz * fz) * dist * 0.9;
        light += S.c0.rgb * 0.05 * dist;
        light += mix(S.c0.rgb, S.c3.rgb, 0.5) * exp(-below * 14.0) * 0.35 * (0.4 + S.k.w);
    } else {
        let up = -below;
        // sun
        let sun_c = vec2<f32>(0.0, hz + 0.13);
        let sr = 0.2;
        let sd = length(uv - sun_c);
        let h = (uv.y - (sun_c.y - sr)) / (2.0 * sr);
        let bands = smoothstep(0.0, 0.05, sin((1.0 - h) * (1.0 - h) * 46.0 - t * 0.4) + 1.15 - 2.2 * (1.0 - h));
        let disc = (1.0 - smoothstep(sr - 0.004, sr + 0.004, sd)) * bands;
        let sun_col = mix(S.c2.rgb, S.c1.rgb, clamp(h, 0.0, 1.0));
        light += sun_col * disc * 0.55 * S.k.y * 1.6;
        light += S.c2.rgb * exp(-sd * 5.0) * 0.22 * S.k.y;
        // horizon haze and stars
        light += mix(S.c0.rgb, S.c3.rgb, 0.5) * exp(-up * 10.0) * 0.4 * (0.4 + S.k.w);
        let st = stars(uv, 50.0, t, 4.4, 0.6) + stars(uv + 3.0, 95.0, t, 7.7, 0.6) * 0.6;
        light += vec3<f32>(st) * S.k.z * 1.2 * smoothstep(0.0, 0.2, up) * (1.0 - disc);
    }
    return S.base.rgb + S.a.x * light;
}

fn scene_rain(frag: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let rows = 44.0 / S.a.z;
    let ch = u.res.y / rows;
    let cw = ch * 0.62;
    let c = floor(frag / vec2<f32>(cw, ch));
    let f = fract(frag / vec2<f32>(cw, ch));
    let col_on = step(hash21(vec2<f32>(c.x, 3.0)), 0.25 + 0.75 * S.k.x);
    var b = 0.0;
    var headb = 0.0;
    for (var k = 0; k < 2; k++) {
        let fk = f32(k);
        let rnd = hash22(vec2<f32>(c.x, 13.0 + fk * 5.0));
        let speed = 0.035 + rnd.x * 0.06;
        let span = rows * (1.6 + fk * 0.7);
        let head = fract(t * speed + rnd.y) * span - 0.15 * rows;
        let dist = head - c.y;
        let tail = (6.0 + 26.0 * S.k.y) * (0.5 + rnd.x);
        if dist >= 0.0 && dist < tail {
            let v = 1.0 - dist / tail;
            b = max(b, v * v);
        }
        if dist >= 0.0 && dist < 1.0 {
            headb = 1.0;
        }
    }
    // pseudo glyph: a random 4x6 bitmap that reshuffles now and then
    let rate = 0.6 + 5.0 * S.k.z * hash21(c + 2.0);
    let gseed = hash21(c + floor(t * rate + hash21(c) * 10.0) * 0.37);
    let gp = floor(f * vec2<f32>(5.0, 7.0));
    let inside = step(0.5, gp.x) * step(gp.x, 3.5) * step(0.5, gp.y) * step(gp.y, 5.5);
    let bit = step(0.48, hash21(gp + gseed * 91.7 + c * 1.3)) * inside;
    let flick = 0.8 + 0.2 * sin(t * 9.0 + c.y * 3.0 + c.x);
    var light = S.c0.rgb * b * bit * 0.75 * flick;
    light += S.c1.rgb * headb * bit * (0.5 + 1.2 * S.k.w);
    // soft column glow so it reads as light rather than pixels
    light += S.c2.rgb * b * 0.1 + S.c0.rgb * headb * 0.12 * S.k.w;
    let uvy = frag.y / u.res.y;
    let haze = S.c3.rgb * (0.4 + 0.6 * uvy);
    return S.base.rgb + haze * 0.4 + S.a.x * light * col_on;
}

fn ridged(p: vec2<f32>) -> f32 {
    return 1.0 - abs(2.0 * vnoise(p) - 1.0);
}

fn scene_abyss(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let sc = S.a.z;
    let depth = 0.5 - uv.y;
    let falloff = 1.2 + 3.0 * S.k.w;
    var light = S.c3.rgb * exp(-depth * falloff) * 0.6;
    // shafts of light from above
    let src = vec2<f32>(0.2, 1.1);
    let dir = uv - src;
    let ang = atan2(dir.x, -dir.y);
    let ray = fbm(vec2<f32>(ang * 11.0 * sc + t * 0.04, t * 0.06), 3);
    let ray2 = fbm(vec2<f32>(ang * 23.0 * sc - t * 0.03, t * 0.05 + 4.0), 2);
    let shafts = pow(ray, 3.0) * 1.8 + pow(ray2, 4.0) * 0.9;
    light += S.c0.rgb * shafts * exp(-depth * (falloff * 0.7)) * S.k.x * 1.3;
    // caustic web
    let q = uv * 5.0 * sc;
    let w1 = vec2<f32>(vnoise(q * 0.7 + t * 0.07), vnoise(q * 0.7 + 4.0 - t * 0.06));
    let ca = ridged(q + w1 * 1.6 + vec2<f32>(t * 0.05, t * 0.03));
    let cb = ridged(q * 1.3 - w1 * 1.4 + vec2<f32>(-t * 0.04, t * 0.045) + 9.0);
    let caust = pow(min(ca, cb), 5.0);
    light += S.c1.rgb * caust * exp(-depth * (falloff * 0.9)) * S.k.y * 1.1;
    // drifting particles
    let pd = vec2<f32>(sin(t * 0.07) * 0.02 + t * 0.003, t * 0.006);
    let snow = stars(uv + pd, 34.0 * sc, t * 0.6, 2.9, 0.5) + stars(uv + pd * 0.5 + 6.0, 70.0 * sc, t * 0.6, 6.6, 0.5) * 0.7;
    light += S.c2.rgb * snow * S.k.z * 0.9;
    return S.base.rgb + S.a.x * light;
}

fn contour_field(uv: vec2<f32>, t: f32, sc: f32, drift: f32) -> f32 {
    let p = uv * 1.5 * sc;
    let w = vec2<f32>(fbm(p * 0.8 + t * 0.012, 3), fbm(p * 0.8 + 5.2 - t * 0.01, 3));
    return fbm(p + vec2<f32>(t * 0.016, -t * 0.011) + w * (0.3 + 1.2 * drift), 4);
}

fn scene_contour(uv: vec2<f32>, S: SceneP) -> vec3<f32> {
    let t = S.a.y;
    let sc = S.a.z;
    let px = 1.0 / u.res.y;
    let levels = 7.0 + 22.0 * S.k.x;
    let f = contour_field(uv, t, sc, S.k.z);
    let fx = contour_field(uv + vec2<f32>(px, 0.0), t, sc, S.k.z);
    let fy = contour_field(uv + vec2<f32>(0.0, px), t, sc, S.k.z);
    let grad = length(vec2<f32>(fx - f, fy - f)) * levels + 1e-5;
    let dist = abs(fract(f * levels) - 0.5) / grad;   // distance to nearest line, in pixels
    let half_w = 0.4 + 1.6 * S.k.y;
    let line = 1.0 - smoothstep(half_w - 0.6, half_w + 0.6, dist);
    // every fifth line is brighter, and a slow band of light sweeps the levels
    let idx = floor(f * levels + 0.5);
    let major = step(abs(fract(idx / 5.0) - 0.0), 0.05);
    let sweep = exp(-pow((fract(f * 1.5 - t * 0.02) - 0.5) * 6.0, 2.0)) * S.k.w;
    var light = S.c0.rgb * line * (0.1 + 0.14 * major + 0.3 * sweep);
    light += S.c2.rgb * line * sweep * 0.25;
    light += S.c1.rgb * f * f * 0.12;
    return S.base.rgb + S.c3.rgb * 0.25 + S.a.x * light;
}

// ---------------------------------------------------------------- main

fn render(frag0: vec2<f32>, S: SceneP) -> vec3<f32> {
    let res = u.res.xy;
    var uv = (frag0 - 0.5 * res) / res.y;
    uv.y = -uv.y;

    // A keystroke sends a ring out from the caret. It bends the scene as it
    // passes and lifts the light there, so every scene answers in its own way.
    var warp = vec2<f32>(0.0);
    var glow = 0.0;
    for (var i = 0; i < 4; i++) {
        let p = u.p[i];
        if p.w <= 0.0 {
            continue;
        }
        var c = (p.xy * res - 0.5 * res) / res.y;
        c.y = -c.y;
        let d = uv - c;
        let r = length(d);
        let life = exp(-p.z * 3.4) * p.w;
        let x = (r - 0.02 - p.z * 0.6) * 17.0;
        let ring = exp(-x * x);
        warp += d / (r + 0.001) * ring * life * 0.035;
        glow += (ring + exp(-r * r * 120.0) * exp(-p.z * 6.0) * 0.7) * life;
    }
    uv -= warp;
    let frag = frag0 - vec2<f32>(warp.x, -warp.y) * res.y;
    let kind = i32(S.a.w + 0.5);
    var col: vec3<f32>;
    switch kind {
        case 1 { col = scene_aurora(uv, S); }
        case 2 { col = scene_starfield(uv, S); }
        case 3 { col = scene_nebula(uv, S); }
        case 4 { col = scene_embers(uv, S); }
        case 5 { col = scene_grid(uv, S); }
        case 6 { col = scene_rain(frag, S); }
        case 7 { col = scene_abyss(uv, S); }
        case 8 { col = scene_contour(uv, S); }
        default { col = scene_plain(uv, S); }
    }
    // lift what is there, and add a little of the scene's own colour so the
    // ring also shows where the scene is nearly black
    let tint = mix(S.c0.rgb, S.c1.rgb, 0.6);
    return col * (1.0 + glow * 2.2) + tint * glow * 0.9;
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let fade = u.res.z;
    var col: vec3<f32>;
    if fade <= 0.0 {
        col = render(pos.xy, u.sa);
    } else if fade >= 1.0 {
        col = render(pos.xy, u.sb);
    } else {
        col = mix(render(pos.xy, u.sa), render(pos.xy, u.sb), fade);
    }
    return vec4<f32>(max(col, vec3<f32>(0.0)), 1.0);
}
