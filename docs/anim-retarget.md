# 姿态重定向（Retargeting，#560 P5）

把一条在**源骨架**上求值出来的姿态搬到**目标骨架**上播放。当前只做 FK 这一半：
没有 IK goal / pole vector、没有 `IKRig` / `RetargeterDef` 资产、没有链类型映射
（Interpolated / OneToOne / Reversed / None）—— 那些要先有 `prism-asset` 的新资产类型。

数学在 `crates/prism-anim-retarget`，不碰 ECS / 渲染 / 资产，和 `prism-anim-ik`、
`prism-motion-matching` 一样是纯值层。

## 映射

```rust
let map = RetargetMap::by_name(&source, &target);       // 同名关节
let map = RetargetMap::new(vec![JointPair::new(3, 7)]); // 显式配对
let pose = retarget(&source, &source_pose, &target, &map, &options)?;
```

- `by_name` 覆盖「同一套 rig 重新导出成不同比例」这种常见情形。
- 同一个目标关节被映射两次时保留第一条，后面的忽略（不静默改覆盖顺序）。
- **没被映射的目标关节保持 rest pose**：只映射 FK 链也能产出可播放的姿态。

## 传输规则

对每一对关节：

| 量 | 规则 |
|---|---|
| 旋转 | 默认相对两个 rest pose：`rest_target · rest_source⁻¹ · anim_source`；`relative_to_rest = false` 时直接拷贝局部旋转（适用于专门为这对骨架做过的 retarget pose） |
| 位移 | 默认按「本帧长度 / 源 rest 长度」的比例缩放目标 rest 位移，即**保留目标骨架自己的骨长**；`scale_translation = false` 直接用 rest |
| 根关节 | 默认**不**传根位移（角色移动交给 gameplay / root motion）；`transfer_root = true` 时才带上 |
| 缩放通道 | 直接拷贝源关节的 scale |
| 全局缩放 | `global_scale` 乘在位移上（角色身高差），默认 `1.0` 保持目标比例 |

为什么不直接拷位移：矮角色播高角色的走路会把腿拉长、脚离地；按 rest 长度比例缩放后，
弯曲角度保留、骨长仍是目标自己的。

## 失败即返回 `None`

映射越界，或姿态长度不等于源骨架关节数 → `None`。半截姿态会被当成动画 bug，
而不是调用方参数错误。

## 还没做

- IK goals / pole vector / PBIK、非 retarget 骨骼的 IK 收敛。
- 链类型映射与 retarget pose 资产（`RetargeterDef`）。
- 编辑器里双骨架并排预览、误差阈值校验（脚 / 手不穿插）。
