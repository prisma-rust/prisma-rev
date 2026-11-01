//! # prism-anim
//!
//! 骨骼、动画剪辑与蒙皮求值（#560 P0-2）。
//!
//! 这一层只做数学，不依赖 engine / render / ECS，因此可以被引擎、编辑器和
//! 离线工具共用：
//!
//! - [`Skeleton`] —— 从 cooked 的 `SkeletonInfo` 建的层次（父必在子前），带
//!   reference（bind）姿态与 `inverse_bind` 矩阵；
//! - [`Clip`] / [`Track`] —— 从 `AnimClipInfo` 来的关键帧轨道；
//! - [`Pose`] —— 每个骨骼的作者空间局部 TRS；
//! - [`Skeleton::bone_palette`] —— `world(pose) * inverse_bind`，GPU 蒙皮与
//!   CPU 参考实现共用；
//! - [`skinning`] —— CPU 参考蒙皮（GPU 路径的 golden 基准）。
//!
//! 约定：矩阵列主序（与 `prism-render` / 引擎一致），四元数 `xyzw`，右手 +Y up。

pub mod clip;
pub mod pose;
pub mod skeleton;
pub mod skinning;

pub use clip::{sample_quat, sample_vec3, Clip, QuatKey, Track, Vec3Key};
pub use pose::Pose;
pub use skeleton::{Joint, JointTransform, Skeleton};
pub use skinning::{palette_to_arrays, skin_matrices, skin_normals, skin_positions};
