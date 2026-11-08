//! # prism-anim-ik
//!
//! IK 求解器（#560 P4）。和 `prism-anim` 一样只做数学，不依赖 engine / render /
//! ECS，因此引擎、编辑器和离线工具可以共用同一份实现。
//!
//! 当前提供：
//!
//! - [`two_bone::solve_two_bone`] —— 解析式两骨 IK：给定髋 / 膝 / 踝（或肩 / 肘 /
//!   腕）的位置、目标与 pole 方向，用余弦定理算出新的膝 / 踝位置和两级旋转；
//! - [`two_bone::apply_two_bone_ik`] —— 把上面的世界空间增量转成两条骨骼的 local
//!   旋转并写回 `Pose`，权重按 slerp 混合；
//! - [`foot::FootPlacement`] —— 逐帧向地面探测两只脚、plant / unplant、按需下调骨盆，
//!   再对每条腿调用两骨 IK。地面查询由调用方注入（引擎里是物理射线）。
//!
//! 约定与 `prism-anim` 一致：矩阵列主序、四元数 `xyzw`、右手 +Y up。所有位置都在
//! **组件空间**（也就是 `Skeleton::world_matrices` 输出的空间），不是世界空间。

pub mod foot;
pub mod two_bone;

pub use foot::{
    FootPlacement, FootPlacementReport, FootPlacementSettings, FootState, GroundHit, GroundQuery,
};
pub use two_bone::{apply_two_bone_ik, solve_two_bone, TwoBoneChain, TwoBoneSolution};
