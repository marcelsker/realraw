// Tone pipeline compute shader for the realraw develop / export stage.
//
// Pipeline (one compute dispatch per tone pass, per (re)tone):
//   in:  src_tex        -- Rgba32Float linear, w*h pixels, top-left origin.
//                          Each texel is one linear f32 RGB pixel (alpha unused).
//   in:  out_tex        -- Rgba8Unorm storage, w'*h' pixels (view-size or
//                          full-resolution). Bytes are sRGB-encoded; the
//                          texture is viewed as Rgba8UnormSrgb when registered
//                          with egui so the GPU applies sRGB->linear at sample
//                          time, undoing our explicit gamma and landing in
//                          display-linear sRGB.
//   in:  params         -- uniform ToneParams (see `ToneParamsGpu`).
//   in:  out_w, out_h   -- output dimensions.
//   in:  src_w, src_h   -- input dimensions.
//   in:  apply_orient   -- 0 = identity, else EXIF orientation (1..=8).
//   in:  apply_downscale-- 0 = 1:1 copy of the source, 1 = nearest, 2 = box.
//
// Group size: 8x8. Dispatch ceil(out_w/8) x ceil(out_h/8).

struct ToneParams {
    // Linear exposure gain, in EV stops. Applied to linear RGB.
    exposure: f32,
    // Temp slider (-1..1). Identity 0.
    temp: f32,
    // Tint slider (-1..1). Identity 0.
    tint: f32,
    // Saturation slider (-1..1). 0 = identity.
    saturation: f32,
    // Contrast slider (-1..1). 0 = identity.
    contrast: f32,
    // Highlights / shadows / whites / blacks (-1..1).
    highlights: f32,
    shadows: f32,
    whites: f32,
    blacks: f32,
    // Output dimensions (in texels).
    out_w: u32,
    out_h: u32,
    // Source dimensions (in texels).
    src_w: u32,
    src_h: u32,
    // EXIF orientation 1..=8 (or 0/1 for identity).
    apply_orient: u32,
    // 0 = copy 1:1, 1 = nearest downscale, 2 = box downscale.
    apply_downscale: u32,
    // _pad: keep 16-byte alignment.
    _pad: u32,
};

@group(0) @binding(0) var<uniform> params: ToneParams;
@group(0) @binding(1) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;

// ---- Per-pixel tone math (matches `crate::develop::pipeline` up to LSB) ---

// Rec.709 linear luminance.
fn lum(r: f32, g: f32, b: f32) -> f32 {
    return 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
}

// Forward sRGB OETF (linear -> sRGB-encoded), matches `srgb_apply_gamma`.
fn srgb_oetf(v: f32) -> f32 {
    if v <= 0.0031308 {
        return v * 12.92;
    } else {
        return 1.055 * pow(v, 1.0 / 2.4) - 0.055;
    }
}

// Inverse sRGB OETF (sRGB-encoded -> linear), matches `srgb_eotf`.
fn srgb_eotf(v: f32) -> f32 {
    if v <= 0.04045 {
        return v / 12.92;
    } else {
        return pow((v + 0.055) / 1.055, 2.4);
    }
}

fn sigmoid(v: f32) -> f32 {
    return 1.0 / (1.0 + exp(-v));
}

fn s_curve(x: f32, k: f32) -> f32 {
    let a  = sigmoid(k * (x - 0.5));
    let a0 = sigmoid(k * -0.5);
    let a1 = sigmoid(k *  0.5);
    return clamp((a - a0) / (a1 - a0), 0.0, 1.0);
}

fn contrast_curve(x: f32, t: f32) -> f32 {
    if t >= 0.0 {
        let k = 1.0 + 2.2 * t;
        let s = s_curve(x, k);
        return x + t * (s - x);
    } else {
        let slope = 1.0 + 0.45 * t;
        return clamp(0.5 + (x - 0.5) * slope, 0.0, 1.0);
    }
}

fn low_mask(x: f32, power: f32) -> f32 {
    return pow(clamp(1.0 - x, 0.0, 1.0), power);
}

fn high_mask(x: f32, power: f32) -> f32 {
    return pow(clamp(x, 0.0, 1.0), power);
}

fn region_curve(x: f32, highlights: f32, shadows: f32, whites: f32, blacks: f32) -> f32 {
    let h = clamp(highlights, -1.0, 1.0);
    let s = clamp(shadows,   -1.0, 1.0);
    let w = clamp(whites,    -1.0, 1.0);
    let b = clamp(blacks,    -1.0, 1.0);
    if abs(h) < 1e-6 && abs(s) < 1e-6 && abs(w) < 1e-6 && abs(b) < 1e-6 {
        return x;
    }
    var y = x;
    if abs(s) > 1e-6 {
        y = clamp(y + s * 0.42 * low_mask(y, 2.0),  0.0, 1.0);
    }
    if abs(h) > 1e-6 {
        y = clamp(y + h * 0.42 * high_mask(y, 2.0), 0.0, 1.0);
    }
    if abs(b) > 1e-6 {
        y = clamp(y + b * 0.32 * low_mask(y, 3.0),  0.0, 1.0);
    }
    if abs(w) > 1e-6 {
        y = clamp(y + w * 0.32 * high_mask(y, 3.0), 0.0, 1.0);
    }
    return y;
}

// Apply the full light panel (contrast + H/S/W/B) on top of sRGB-encoded
// luminance, rescaling channels to preserve chromaticity.
fn apply_light(r: f32, g: f32, b: f32, p: ToneParams) -> vec3<f32> {
    let needs_curve = abs(p.contrast)   > 1e-6
                   || abs(p.highlights) > 1e-6
                   || abs(p.shadows)   > 1e-6
                   || abs(p.whites)    > 1e-6
                   || abs(p.blacks)    > 1e-6;
    if !needs_curve {
        return vec3<f32>(r, g, b);
    }
    let y = lum(r, g, b);
    if y <= 1e-10 {
        return vec3<f32>(max(r, 0.0), max(g, 0.0), max(b, 0.0));
    }
    var ye = srgb_oetf(clamp(y, 0.0, 1.0));
    if abs(p.contrast) > 1e-6 {
        ye = contrast_curve(ye, p.contrast);
    }
    ye = region_curve(ye, p.highlights, p.shadows, p.whites, p.blacks);
    let y2 = srgb_eotf(ye);
    let scale = y2 / y;
    return vec3<f32>(max(r * scale, 0.0), max(g * scale, 0.0), max(b * scale, 0.0));
}

fn apply_saturation(r: f32, g: f32, b: f32, sat: f32) -> vec3<f32> {
    if abs(sat) <= 1e-6 {
        return vec3<f32>(r, g, b);
    }
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let k = 1.0 + sat;
    return vec3<f32>(
        clamp(y + (r - y) * k, 0.0, 1.0),
        clamp(y + (g - y) * k, 0.0, 1.0),
        clamp(y + (b - y) * k, 0.0, 1.0),
    );
}

// ---- Source sampling: EXIF orientation + optional downscale ----

// EXIF orientation remap. (sx, sy) in source-space is mapped to (ox, oy)
// in oriented source-space. Oriented src is then re-mapped to (ox, oy)
// in output-space via a nearest or box downscale.
//
// Returns the source-space coordinates to sample from for an output texel
// at (ox, oy) where ox, oy are in output-texel space (0..out_w/h).
fn exif_remap(ox: i32, oy: i32, orient: u32, src_w: i32, src_h: i32) -> vec2<i32> {
    // EXIF values follow the `apply_orientation_rgb` Rust cases.
    // Oriented dimensions:
    //  1, 2, 3, 4 -> src_w x src_h
    //  5, 6, 7, 8 -> src_h x src_w (rotated 90)
    var w = src_w;
    var h = src_h;
    if orient >= 5u && orient <= 8u {
        w = src_h;
        h = src_w;
    }
    // Map (ox, oy) from oriented space -> source space.
    var sx = ox;
    var sy = oy;
    switch (orient) {
        case 1u, 0u: { /* identity */ }
        case 2u: { sx = src_w - 1 - ox; }                  // flip H
        case 3u: { sx = src_w - 1 - ox; sy = src_h - 1 - oy; } // 180
        case 4u: { sy = src_h - 1 - oy; }                  // flip V
        case 5u: { let t = ox; sx = src_h - 1 - oy; sy = t; }     // transpose
        case 6u: { let t = ox; sx = oy; sy = src_w - 1 - t; }     // 90 CW
        case 7u: { let t = ox; sx = oy; sy = t; }                 // transpose+flipH
        case 8u: { let t = ox; sx = src_h - 1 - oy; sy = src_w - 1 - t; } // 90 CCW
        default: {}
    }
    return vec2<i32>(clamp(sx, 0, w - 1), clamp(sy, 0, h - 1));
}

fn sample_nearest(ox: i32, oy: i32, p: ToneParams) -> vec4<f32> {
    // We need oriented src dims. For nearest + 1:1 with no orient, the
    // output is just src. For downscale, out space is smaller than oriented
    // space.
    // Nearest scaling: out_texel (ox, oy) maps to oriented (ox * ow / out_w, oy * oh / out_h).
    // For nearest: take one sample at that oriented texel.
    // We sample the source directly: inverse through EXIF.
    let ow = i32(p.out_w);
    let oh = i32(p.out_h);
    // Re-derive in a switch on downscale to keep types concrete:
    var src_xy: vec2<i32>;
    if p.apply_downscale == 0u {
        // 1:1 in oriented space; (ox, oy) in output == (ox, oy) in oriented src.
        src_xy = exif_remap(ox, oy, p.apply_orient, i32(p.src_w), i32(p.src_h));
    } else {
        // Nearest: oriented (ox*src_w/out_w, oy*src_h/out_h), then EXIF.
        let ox_o = ox * i32(p.src_w) / ow;
        let oy_o = oy * i32(p.src_h) / oh;
        src_xy = exif_remap(ox_o, oy_o, p.apply_orient, i32(p.src_w), i32(p.src_h));
    }
    return textureLoad(src_tex, vec2<i32>(src_xy.x, src_xy.y), 0);
}

fn sample_box(ox: i32, oy: i32, p: ToneParams) -> vec4<f32> {
    // Box-filter 2x2 downscale matches the CPU `downscale_rgb_with_progress`.
    // For each output texel we average 4 source texels in oriented space.
    let ow = i32(p.out_w);
    let oh = i32(p.out_h);
    let ox0 = ox * i32(p.src_w) / ow;
    let oy0 = oy * i32(p.src_h) / oh;
    let ox1 = max((ox + 1) * i32(p.src_w) / ow, ox0 + 1);
    let oy1 = max((oy + 1) * i32(p.src_h) / oh, oy0 + 1);

    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    var count = 0.0;
    for (var sy = oy0; sy < oy1; sy = sy + 1) {
        for (var sx = ox0; sx < ox1; sx = sx + 1) {
            let p_ = exif_remap(sx, sy, p.apply_orient, i32(p.src_w), i32(p.src_h));
            acc = acc + textureLoad(src_tex, vec2<i32>(p_.x, p_.y), 0);
            count = count + 1.0;
        }
    }
    if count > 0.0 {
        return acc / count;
    }
    return vec4<f32>(0.0);
}

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ox = i32(gid.x);
    let oy = i32(gid.y);
    if ox >= i32(params.out_w) || oy >= i32(params.out_h) {
        return;
    }

    // Sample (orientation + optional downscale).
    var src: vec4<f32>;
    if params.apply_downscale == 2u {
        src = sample_box(ox, oy, params);
    } else {
        src = sample_nearest(ox, oy, params);
    }
    var r = max(src.r, 0.0);
    var g = max(src.g, 0.0);
    var b = max(src.b, 0.0);

    // Stage 1: WB × exposure.
    let gain = exp2(params.exposure);
    let t = clamp(params.temp, -1.0, 1.0);
    let ti = clamp(params.tint, -1.0, 1.0);
    let r_wb = exp2( t * 0.3);
    let g_wb = exp2( ti * 0.2);
    let b_wb = exp2(-t * 0.3);
    r = r * gain * r_wb;
    g = g * gain * g_wb;
    b = b * gain * b_wb;

    // Clamp before tone curves (CPU pipeline doesn't, but the light panel
    // is bounded in [0,1] anyway; the gamma is monotonic on positive values).
    r = min(r, 1.0);
    g = min(g, 1.0);
    b = min(b, 1.0);

    // Stage 2: light panel (luminance curves in sRGB space).
    let toned = apply_light(r, g, b, params);
    r = toned.r;
    g = toned.g;
    b = toned.b;

    // Stage 3a: sRGB OETF (linear -> sRGB-encoded).
    r = srgb_oetf(r);
    g = srgb_oetf(g);
    b = srgb_oetf(b);

    // Stage 3b: saturation on sRGB-encoded channels.
    let sat = apply_saturation(r, g, b, params.saturation);
    r = sat.r;
    g = sat.g;
    b = sat.b;

    // Quantize to 8-bit. The texture is Rgba8Unorm so the store does the
    // cast; we still need a final clamp for the round() conversion to be
    // well-defined.
    let ru = u32(clamp(r * 255.0 + 0.5, 0.0, 255.0));
    let gu = u32(clamp(g * 255.0 + 0.5, 0.0, 255.0));
    let bu = u32(clamp(b * 255.0 + 0.5, 0.0, 255.0));
    textureStore(out_tex, vec2<i32>(ox, oy), vec4<f32>(
        f32(ru) / 255.0,
        f32(gu) / 255.0,
        f32(bu) / 255.0,
        1.0,
    ));
}
