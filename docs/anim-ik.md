# 动画 IK（`prism-anim-ik`）

`crates/prism-anim-ik` 是 #560 P4 的数学底座：解析式 IK，不依赖 engine / render /
ECS，引擎、编辑器预览和离线工具共用同一份实现。当前只有两骨 IK；Foot Placement、
LegIK、PBIK、IK Rig / Retargeter 是后续切片，落地前不要在其他地方另写一份。

## 约定

- 与 `prism-anim` 一致：右手系、+Y up、相机 −Z；四元数 `xyzw`（`glam::Quat`），
  矩阵列主序。
- 求解在**组件空间**进行 —— 也就是 `Skeleton::world_matrices(pose)` 输出的空间，
  不是世界空间。目标点和 pole 方向都要先换到同一空间；实体自身的
  `LocalToWorld` 不在这一层参与运算。
- 权重 `weight` 是 `0..=1` 的混合系数，`slerp` 与位置 `lerp` 各按它插值；
  `0` 表示不求解（返回 `None`，姿态不变）。

## 两骨 IK

```rust
use prism_anim_ik::{apply_two_bone_ik, solve_two_bone, TwoBoneChain};

// 纯数学：给出三节骨骼的位置、目标与 pole，拿回两级世界空间增量旋转。
let solution = solve_two_bone(hip, knee, foot, target, pole, 1.0);

// 接骨架：把增量转成两条骨骼的 local 旋转写回 Pose。
apply_two_bone_ik(&skeleton, &mut pose, TwoBoneChain::new(0, 1, 2), target, pole, 1.0);
```

- 中间关节的位置用余弦定理求出，弯曲平面由 `pole` 投影到「指向目标」的垂直面上
  决定；`pole` 与目标方向平行时退回与目标方向最不对齐的坐标轴，保证结果稳定。
- 够不到时把链路拉直，太近时按 `|a − b|` 折叠；两种情况都置
  `TwoBoneSolution::clamped`，`reach` 是实际使用的距离。
- 非有限输入、零长度骨骼、目标与根关节重合、骨骼下标重复或越界都返回 `None`，
  不会写出 NaN 姿态。
- `TwoBoneSolution` 的两个旋转是**世界空间增量**（`new = delta * old`）；
  `apply_two_bone_ik` 用父级世界旋转把它们换算成 local 增量，返回的
  `middle_position` / `end_position` 是应用后姿态真正到达的位置（`slerp` 后的
  位置不等于对位置做 `lerp`）。

## 测试

`crates/prism-anim-ik/src/two_bone_tests.rs` 覆盖：命中目标、超出范围拉直、
过近折叠、pole 决定弯曲方向、pole 平行回退、权重混合、各类退化输入、
写回 `Pose` 后的实际位置、权重 0 不动姿态、非法骨骼下标。

## Foot Placement

`FootPlacement`（`src/foot.rs`）按帧把两只脚贴到地面，再让骨盆跟着降下来：

```rust
let report = placement.update(
    &skeleton, &mut pose,
    0,                                  // 骨盆关节
    [left_chain, right_chain],          // thigh -> knee -> ankle
    &settings, &ground_queries, dt,
);
```

- 地面由调用方注入：实现 `GroundQuery::cast(origin, direction, max_distance)` 即可。
  引擎里接物理射线（#522），测试里是平面或台阶。
- 每只脚从踝上方 `probe_height` 向下探测：打不到地面、地面高出 `max_step_up`
  （墙）或低于 `max_step_down`（洞）都视为没地面，权重按 `blend_seconds` 衰减到 0，
  脚留在最后一次的目标位置。
- **骨盆只会下降**：以未叠加修正的骨盆为基准，算出最吃紧那条腿超出的距离，
  向下移动并夹在 `pelvis_min..=0`。往上抬只会让已经着地的脚更够不着，抬脚由
  两骨 IK 自己弯膝解决。修正量按「已烘焙进姿态的偏移」记差，重复调用不会累积。
- 两条腿的 `upper` 必须都挂在骨盆关节下（`parent == hips`）；骨盆本身不被旋转，否则
  两条腿会各自把同一根骨骼转一次。
- `weight = 0` 直接清空状态并保持姿态不变；非法链 / 姿态长度不符返回
  `applied = false`，不写任何骨骼。

## 未接线

引擎还没有消费这个求解器：Foot Placement 需要物理射线查询（#522）与
`CharacterController`（#560 P2），接线切片会把 `TwoBoneChain` 映射到骨架关节名，
逐帧对脚做地面查询后调用 `apply_two_bone_ik`。
