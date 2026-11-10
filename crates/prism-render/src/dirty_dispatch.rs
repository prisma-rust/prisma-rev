//! 统一脏分派计划（`DirtyDispatchPlan`，DESIGN §9 / PR-S2）。
//!
//! 在 `prepare` 阶段一次性消费场景与资产脏标记，按拓扑序批处理：
//! 1. 纹理上传 / 释放（`RenderTextureManager`）
//! 2. 网格上传 / BLAS 重建（`RenderMeshManager`）
//! 3. 材质缓冲区更新（`RenderMaterialManager`）
//! 4. 光源缓冲区更新（`RenderAnalyticLightManager`）
//! 5. 实例/绘制项变化（`RenderInstanceManager`）
//! 6. 天空与发光表级联更新（`RenderSkyManager` / `RenderEmissiveLightTable`）

/// 场景与资源变更的统一调度计划。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyDispatchPlan {
    /// 待上传/更新的纹理数量
    pub texture_uploads: usize,
    /// 待释放的纹理槽数量
    pub texture_releases: usize,
    /// 待上传/重建的网格数量
    pub mesh_uploads: usize,
    /// 待释放的网格句柄数量
    pub mesh_releases: usize,
    /// 待上传/更新的材质数量
    pub material_updates: usize,
    /// 待释放的材质槽数量
    pub material_releases: usize,
    /// 相机是否发生变化（VP/Eye/Projection）
    pub camera_dirty: bool,
    /// 方向光或局部光源是否发生变化
    pub lights_dirty: bool,
    /// 天空盒/IBL 是否发生变化
    pub sky_dirty: bool,
    /// 实例/绘制列表内容是否发生变化
    pub instances_dirty: bool,
    /// 级联发光网格表是否需要重建（由 mesh/material/instances 脏级联触发）
    pub emissive_table_dirty: bool,
    /// 路径追踪累积是否需要重置
    pub pt_accum_dirty: bool,
}

impl DirtyDispatchPlan {
    /// 检查是否有任何资源上传或状态更新需要分派。
    pub fn has_work(&self) -> bool {
        self.texture_uploads > 0
            || self.texture_releases > 0
            || self.mesh_uploads > 0
            || self.mesh_releases > 0
            || self.material_updates > 0
            || self.material_releases > 0
            || self.camera_dirty
            || self.lights_dirty
            || self.sky_dirty
            || self.instances_dirty
            || self.emissive_table_dirty
            || self.pt_accum_dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_has_no_work() {
        let plan = DirtyDispatchPlan::default();
        assert!(!plan.has_work());
    }

    #[test]
    fn plan_detects_individual_work_items() {
        let mut plan = DirtyDispatchPlan::default();
        plan.texture_uploads = 1;
        assert!(plan.has_work());

        let mut plan2 = DirtyDispatchPlan::default();
        plan2.camera_dirty = true;
        assert!(plan2.has_work());

        let mut plan3 = DirtyDispatchPlan::default();
        plan3.pt_accum_dirty = true;
        assert!(plan3.has_work());
    }
}
