//! # 高度图生成器
//!
//! 参数化超高落差拟真水力+热力侵蚀高度图生成器。
//! 支持 −11 000 m 到 +8 850 m（总落差 ≈ 20 km）。
//!
//! ## Usage
//! ```rust,ignore
//! let hm = Heightmap::new(width, height, data);
//! let params = ErosionParams::default();
//! let result = generate_eroded_heightmap(hm, &params);
//! ```

mod hydraulic;
mod terrain;
mod thermal;

pub use terrain::generate_terrain;
pub use thermal::thermal_erosion;

// ---------------------------------------------------------------------------
// Heightmap
// ---------------------------------------------------------------------------

/// 高度图，使用 `f64` 保证 20 km 高差精度。
#[derive(Clone, Debug)]
pub struct Heightmap {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f64>,
    /// 海平面高度（相对高度模式下为 0.0）。
    pub sea_level: f64,
    /// 全局最低点（相对/绝对）。
    pub min_height: f64,
    /// 全局最高点（相对/绝对）。
    pub max_height: f64,
}

impl Heightmap {
    /// 从 `Vec<f64>` 创建，自动计算 min/max。
    pub fn new(width: usize, height: usize, data: Vec<f64>) -> Self {
        assert_eq!(
            data.len(),
            width * height,
            "data length must match width × height"
        );
        let mut min_h = f64::INFINITY;
        let mut max_h = f64::NEG_INFINITY;
        for &v in &data {
            if v < min_h {
                min_h = v;
            }
            if v > max_h {
                max_h = v;
            }
        }
        Self {
            width,
            height,
            data,
            sea_level: 0.0,
            min_height: min_h,
            max_height: max_h,
        }
    }

    /// 获取 (x, y) 处的高度（x ∈ [0, width), y ∈ [0, height)）。
    #[inline]
    pub fn get(&self, x: usize, y: usize) -> f64 {
        self.data[y * self.width + x]
    }

    /// 设置 (x, y) 处的高度，自动更新 min/max。
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, value: f64) {
        let idx = y * self.width + x;
        self.data[idx] = value;
        if value < self.min_height {
            self.min_height = value;
        }
        if value > self.max_height {
            self.max_height = value;
        }
    }

    /// 双线性插值采样（支持浮点坐标）。
    pub fn sample(&self, x: f64, y: f64) -> f64 {
        let w = self.width as f64;
        let h = self.height as f64;
        let x = x.clamp(0.0, w - 1.001);
        let y = y.clamp(0.0, h - 1.001);

        let x0 = x as usize;
        let y0 = y as usize;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = x - x0 as f64;
        let fy = y - y0 as f64;

        let h00 = self.get(x0, y0);
        let h10 = self.get(x1, y0);
        let h01 = self.get(x0, y1);
        let h11 = self.get(x1, y1);

        let top = h00 + (h10 - h00) * fx;
        let bot = h01 + (h11 - h01) * fx;
        top + (bot - top) * fy
    }

    /// 转换为相对高度：所有值减去 `min_height`，使最小值变为 0。
    pub fn to_relative(&mut self) {
        let min = self.min_height;
        if min == 0.0 {
            return;
        }
        for v in self.data.iter_mut() {
            *v -= min;
        }
        self.max_height -= min;
        self.min_height = 0.0;
    }

    /// 恢复绝对高度：所有值加上保存的最小值。
    pub fn from_relative(&mut self, saved_min: f64) {
        if saved_min == 0.0 {
            return;
        }
        for v in self.data.iter_mut() {
            *v += saved_min;
        }
        self.min_height += saved_min;
        self.max_height += saved_min;
    }

    /// 无量纲化：缩放到 [0, 1] 范围。
    pub fn normalize(&mut self) {
        let range = self.max_height - self.min_height;
        if range <= 0.0 {
            return;
        }
        for v in self.data.iter_mut() {
            *v = (*v - self.min_height) / range;
        }
        self.max_height = 1.0;
        self.min_height = 0.0;
    }

    /// 从 [0, 1] 还原到指定范围。
    pub fn denormalize(&mut self, new_min: f64, new_max: f64) {
        let range = new_max - new_min;
        for v in self.data.iter_mut() {
            *v = *v * range + new_min;
        }
        self.min_height = new_min;
        self.max_height = new_max;
    }
}

// ---------------------------------------------------------------------------
// ErosionParams
// ---------------------------------------------------------------------------

/// 侵蚀参数，所有关键物理参数可实时调节。
#[derive(Clone, Debug)]
pub struct ErosionParams {
    // ---- 水力参数 ----
    pub inertia: f64,
    pub capacity_factor: f64,
    pub min_slope: f64,
    pub deposition_rate: f64,
    pub erosion_rate: f64,
    pub evaporation_rate: f64,
    pub gravity: f64,
    /// 速度硬上限，防止超高速发散（关键参数）。
    pub max_velocity: f64,
    /// 沉积容量硬上限（关键参数）。
    pub max_capacity: f64,

    // ---- 热力参数 ----
    /// 休止角（度）。
    pub talus_angle: f64,
    pub thermal_strength: f64,

    // ---- 全局控制 ----
    pub iterations: usize,
    pub particle_count: usize,
    pub max_steps: usize,
    pub cell_size: f64,
    /// 随机数种子（用于水力侵蚀粒子生成与随机漫步，保证相同参数输出确定性）。
    pub seed: u64,
    /// 热力侵蚀间隔（水力每 N 轮后做一次热力）。
    pub thermal_interval: usize,
    /// 预处理热力轮数。
    pub thermal_pre_iterations: usize,
    /// 海平面以下侵蚀倍率（0.0 ~ 1.0）。
    pub sea_erosion_factor: f64,

    // ---- 输出控制 ----
    pub use_relative_height: bool,
}

impl Default for ErosionParams {
    fn default() -> Self {
        Self {
            // 水力
            inertia: 0.05,
            capacity_factor: 4.0,
            min_slope: 0.01,
            deposition_rate: 0.3,
            erosion_rate: 0.3,
            evaporation_rate: 0.01,
            gravity: 4.0,
            max_velocity: 20.0,
            max_capacity: 10.0,
            // 热力
            talus_angle: 35.0,
            thermal_strength: 0.5,
            // 控制
            iterations: 100,
            particle_count: 200_000,
            max_steps: 150,
            cell_size: 1.0,
            seed: 0x5EED,
            thermal_interval: 5,
            thermal_pre_iterations: 100,
            sea_erosion_factor: 0.05,
            // 输出
            use_relative_height: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Particle
// ---------------------------------------------------------------------------

/// 水力侵蚀粒子。
#[derive(Clone, Debug)]
pub(crate) struct Particle {
    pub x: f64,
    pub y: f64,
    pub velocity_x: f64,
    pub velocity_y: f64,
    pub water: f64,
    pub sediment: f64,
    pub speed: f64,
    pub seed: u64,
}

impl Particle {
    pub fn new(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            velocity_x: 0.0,
            velocity_y: 0.0,
            water: 1.0,
            sediment: 0.0,
            speed: 0.0,
            seed: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// 执行完整侵蚀管线：热力预处理 → 水力+热力交替迭代。
///
/// 返回侵蚀后的高度图。若 `params.use_relative_height` 为 true，
/// 内部转换为相对高度计算，输出时保留相对高度。否则恢复原始绝对高度。
pub fn generate_eroded_heightmap(mut hm: Heightmap, params: &ErosionParams) -> Heightmap {
    let saved_min = hm.min_height;

    // 转为相对高度（可选）
    if params.use_relative_height {
        hm.to_relative();
    }

    // 第一阶段：强力热力预处理
    for _ in 0..params.thermal_pre_iterations {
        thermal_erosion(&mut hm, params);
    }

    // 第二阶段：水力 + 热力交替迭代
    //
    // 累积缓冲（每线程一张整图）在整条管线里复用：默认 100 轮水力侵蚀不再
    // 每轮重新分配一份「线程数 × 整图」的 `f64` 表。
    let mut scratch = hydraulic::ErosionScratch::new();
    for iter in 0..params.iterations {
        let round_seed = params
            .seed
            .wrapping_add((iter as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        hydraulic::hydraulic_erosion_with_scratch(&mut hm, params, &mut scratch, round_seed);
        if params.thermal_interval > 0 && (iter + 1) % params.thermal_interval == 0 {
            thermal_erosion(&mut hm, params);
        }
    }

    // 恢复绝对高度（可选）
    if !params.use_relative_height {
        hm.from_relative(saved_min);
    }

    hm
}

/// 双缓冲拷贝
pub(crate) fn clone_data(data: &[f64]) -> Vec<f64> {
    data.to_vec()
}

#[cfg(test)]
mod tests;
