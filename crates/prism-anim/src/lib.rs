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
//! - [`blend`] —— 姿态混合与一维混合空间（动画图 / BlendStack 的地基）；
//! - [`skinning`] —— CPU 参考蒙皮（GPU 路径的 golden 基准）。
//!
//! 约定：矩阵列主序（与 `prism-render` / 引擎一致），四元数 `xyzw`，右手 +Y up。

pub mod blend;
pub mod blend_space2d;
pub mod blend_stack;
pub mod clip;
pub mod graph_eval;
pub mod montage;
pub mod pose;
pub mod skeleton;
pub mod skinning;
pub mod state_machine;
pub mod sync;

pub use blend::{
    additive_joint, apply_additive, blend_into, blend_joint, blend_poses, blend_space_weights,
    BlendSample, BlendSpace1D,
};
pub use blend_space2d::{BlendSample2D, BlendSpace2D};
pub use blend_stack::{BlendLayer, BlendStack, BoneMask};
pub use clip::{sample_quat, sample_vec3, Clip, ClipNotify, QuatKey, Track, Vec3Key};
pub use graph_eval::{
    AnimGraphEvaluator, AnimGraphSource, GraphEvalError, GraphNodeSnapshot, GraphNotifyEvent,
    MontageSnapshot,
};
pub use montage::{Montage, MontageNotify, MontagePlayer, MAX_MONTAGE_QUEUE};
pub use pose::Pose;
pub use prism_asset::formats_anim_graph::GraphNodeKind;
pub use prism_asset::formats_skinned::{
    decode_ranim, AnimClipInfo, AnimNotifyInfo, AnimTrackInfo, QuatKeyInfo, Vec3KeyInfo,
};
pub use skeleton::{Joint, JointTransform, Skeleton};
pub use skinning::{palette_to_arrays, skin_matrices, skin_normals, skin_positions};
pub use state_machine::{
    AnimParameters, AnimState, AnimTransition, StateMachine, TransitionCondition, TransitionTarget,
    ANY_STATE,
};
pub use sync::{SyncGroup, SyncMarkerSet};
