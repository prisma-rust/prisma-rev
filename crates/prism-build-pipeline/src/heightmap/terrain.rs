//! 初始地形生成：seed 化值噪声 + FBM。
//!
//! 产生 [0, 1] 相对高度场，调用方可用 [`Heightmap::denormalize`]
//! 映射到真实高程范围（如 −11 000 m ~ +8 850 m）。

use super::Heightmap;

// ---------------------------------------------------------------------------
// Hash / noise
// ---------------------------------------------------------------------------

/// splitmix64 finalizer: strong avalanche with pure integer math.
#[inline]
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Hash a lattice point to [0, 1).
///
/// Integer hashing keeps the value identical across platforms. The previous
/// `sin(dot) * 43758.5453` trick depended on the platform libm and lost
/// precision on large coordinates, so one seed produced different terrain on
/// Windows, Linux and Android.
#[inline]
fn hash2(p: [f64; 2], seed: u64) -> f64 {
    let x = p[0] as i64 as u64;
    let y = p[1] as i64 as u64;
    let mut h = seed ^ 0x9E37_79B9_7F4A_7C15;
    h = mix64(h ^ x);
    h = mix64(h ^ y);
    // 53 bits is exactly the f64 mantissa width, so the result stays uniform.
    (h >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// 2D 值噪声，返回 [−1, 1]。
#[inline]
fn noise_2d(p: [f64; 2], seed: u64) -> f64 {
    let i = [p[0].floor(), p[1].floor()];
    let w = [p[0] - i[0], p[1] - i[1]];
    // 平滑插值（3次 Hermite）。
    let u = [
        w[0] * w[0] * (3.0 - 2.0 * w[0]),
        w[1] * w[1] * (3.0 - 2.0 * w[1]),
    ];

    let a = hash2(i, seed);
    let b = hash2([i[0] + 1.0, i[1]], seed);
    let c = hash2([i[0], i[1] + 1.0], seed);
    let d = hash2([i[0] + 1.0, i[1] + 1.0], seed);

    -1.0 + 2.0 * (a + (b - a) * u[0] + (c - a) * u[1] + (a - b - c + d) * u[0] * u[1])
}

/// 分形布朗运动：多倍频叠加，返回约 [−1, 1]。
///
/// `gain` — 每倍频振幅衰减（典型 0.5）；`lacunarity` — 频率倍率（典型 2.0）。
fn fbm(p: [f64; 2], octaves: u32, gain: f64, lacunarity: f64, seed: u64) -> f64 {
    let mut p = p;
    let mut amplitude = 1.0;
    let mut total = 0.0;
    let mut norm = 0.0;
    for _ in 0..octaves {
        total += amplitude * noise_2d(p, seed);
        norm += amplitude;
        amplitude *= gain;
        p = [p[0] * lacunarity, p[1] * lacunarity];
    }
    total / norm
}

// ---------------------------------------------------------------------------
// 地形合成
// ---------------------------------------------------------------------------

/// 生成 [0, 1] 相对高度的初始地形。
///
/// 频率按**物理地块尺度**标定：`scale_m` 是地块宽度（米）。
/// 512 m 地块基准（`freq_scale = 1.0`）：
/// - 低频骨架 `base`（~1.5 周期）：大尺度起伏约 340 m
/// - 山脊层 `ridge` = 1 − |fbm|（~2.5 周期）：山脊间隔约 200 m
/// - 细节层 `detail`（~10 周期）：谷地纹理约 50 m
/// - 强域扭曲让山脊自然蜿蜒
///
/// 地块越大频率越低，保持相同的物理特征周期。
pub fn generate_terrain(width: usize, height: usize, seed: u64, scale_m: f64) -> Heightmap {
    // 频率标度：512 m 地块 → 1.0；1024 m → 0.5（相同物理周期）。
    let freq = 512.0 / scale_m.max(1.0);

    // 每层用不同 seed 相位，避免各倍频对齐产生网格感。
    let seed_a = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let seed_b = seed.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let seed_c = seed.wrapping_mul(0x94D0_49BB_1331_11EB);

    let mut data = Vec::with_capacity(width * height);
    for y in 0..height {
        let ny = y as f64 / height as f64;
        for x in 0..width {
            let nx = x as f64 / width as f64;
            // 域扭曲：低频噪声偏移采样坐标，山脊走向随之蜿蜒。
            let warp = 0.5 * fbm([nx * 2.0, ny * 2.0], 4, 0.5, 2.0, seed_a);
            let q = [nx + warp, ny + warp];

            let base = fbm([q[0] * 1.5 * freq, q[1] * 1.5 * freq], 5, 0.5, 2.0, seed_a);
            // 山脊噪声：|fbm| 取反 → f=0 处为脊线（1.0），两侧滑落（0.0）。
            let ridge =
                1.0 - fbm([q[0] * 2.5 * freq, q[1] * 2.5 * freq], 5, 0.5, 2.0, seed_b).abs();
            let detail = fbm(
                [q[0] * 10.0 * freq, q[1] * 10.0 * freq],
                3,
                0.5,
                2.0,
                seed_c,
            );

            // base ∈ [−1,1]；ridge ∈ [0,1] → 平移/缩放回 [−1,1]；detail ∈ [−1,1]
            let e = 0.40 * base + 0.45 * (2.0 * ridge - 1.0) + 0.15 * detail;
            data.push(e);
        }
    }

    let mut hm = Heightmap::new(width, height, data);
    // 归一化到 [0, 1]，保证不同 seed 输出范围一致。
    hm.normalize();
    hm
}

#[cfg(test)]
#[path = "terrain_tests.rs"]
mod tests;
