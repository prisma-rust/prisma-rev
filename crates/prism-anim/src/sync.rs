//! 同步组 / Sync marker（#560 P1）。
//!
//! 走路、跑步这类循环剪辑的「相位」不能只按归一化时间对齐：脚落地的位置在
//! 不同剪辑里可能落在 0.18 / 0.62，而不是同一个 0.0。同步组用一组标记把
//! **领跑剪辑的相位**分段映射到**跟随剪辑的相位**，让两只脚在混合时踩在同一
//! 个节拍上。
//!
//! 这一层只做数学：标记是归一化相位 `[0, 1)`，不依赖剪辑 / 资产 / ECS。
//! 图求值器和混合空间可以在采样前调用 [`SyncGroup::map_phase`]。
//!
//! 约定：
//!
//! - 每个剪辑的标记数量必须相同；标记按相位升序、重复值去重；
//! - 映射在相邻标记之间线性插值，最后一段从最后一个标记回绕到第一个标记；
//! - 标记只有 1 个时，所有相位都映射到该标记；
//! - 非有限相位、越界剪辑下标、非法组都安全回退（返回领跑相位或 `None`）。

/// 一个剪辑的同步标记，相位归一化到 `[0, 1)`。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncMarkerSet {
    phases: Vec<f32>,
}

impl SyncMarkerSet {
    /// 从标记相位构造。非有限值丢弃，相位按 `rem_euclid(1)` 归一化，
    /// 排序后去掉重复值；空集合返回 `None`。
    pub fn new(phases: impl IntoIterator<Item = f32>) -> Option<Self> {
        let mut phases: Vec<f32> = phases
            .into_iter()
            .filter(|phase| phase.is_finite())
            .map(|phase| phase.rem_euclid(1.0))
            .collect();
        phases.sort_by(f32::total_cmp);
        phases.dedup_by(|a, b| (*a - *b).abs() <= 1e-6);
        if phases.is_empty() {
            None
        } else {
            Some(Self { phases })
        }
    }

    /// 升序、去重后的标记相位。
    pub fn phases(&self) -> &[f32] {
        &self.phases
    }

    pub fn len(&self) -> usize {
        self.phases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.phases.is_empty()
    }
}

/// 一组共享标记结构的剪辑。下标 0 通常作为领跑剪辑，但 [`Self::map_phase`]
/// 允许任意两个下标互转。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncGroup {
    sets: Vec<SyncMarkerSet>,
}

impl SyncGroup {
    /// 构造同步组；所有剪辑的标记数量必须相同且非空。
    pub fn new(sets: Vec<SyncMarkerSet>) -> Option<Self> {
        let count = sets.first()?.len();
        if sets.iter().any(|set| set.len() != count) {
            None
        } else {
            Some(Self { sets })
        }
    }

    pub fn len(&self) -> usize {
        self.sets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    pub fn markers(&self, clip: usize) -> Option<&SyncMarkerSet> {
        self.sets.get(clip)
    }

    /// 把 `from` 剪辑的归一化相位映射到 `to` 剪辑。
    ///
    /// 越界下标 / 非有限相位时原样返回 `phase`（调用方仍能继续采样，
    /// 不会因为一条坏标记把整个混合打断）。
    pub fn map_phase(&self, from: usize, to: usize, phase: f32) -> f32 {
        if !phase.is_finite() || from >= self.sets.len() || to >= self.sets.len() {
            return phase;
        }
        let phase = phase.rem_euclid(1.0);
        let leader = &self.sets[from].phases;
        let follower = &self.sets[to].phases;
        let count = leader.len();
        if count <= 1 {
            return follower[0];
        }
        if from == to {
            return phase;
        }

        // 找到领跑剪辑包含 `phase` 的段；`phase` 在第一个标记之前时属于
        // 最后一段（相位 +1 后与回绕的段比较）。
        let (segment, local) = if phase < leader[0] {
            let start = leader[count - 1];
            let end = leader[0] + 1.0;
            (count - 1, (phase + 1.0 - start) / (end - start))
        } else {
            let mut segment = count - 1;
            for index in 0..count - 1 {
                if phase >= leader[index] && phase < leader[index + 1] {
                    segment = index;
                    break;
                }
            }
            let start = leader[segment];
            let end = if segment + 1 < count {
                leader[segment + 1]
            } else {
                leader[0] + 1.0
            };
            (segment, (phase - start) / (end - start))
        };

        let start = follower[segment];
        let end = if segment + 1 < count {
            follower[segment + 1]
        } else {
            follower[0] + 1.0
        };
        (start + local.clamp(0.0, 1.0) * (end - start)).rem_euclid(1.0)
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
