# 动作匹配（Motion Matching，#560 P3）

从剪辑库里挑「当前姿态 + 期望轨迹」最匹配的一帧继续播放，取代手写状态机里
「速度大于 X 就切到跑」这类阈值。约定与动画系统一致：模型空间 = 骨架根空间，
实体变换最后再乘，`+Y` 朝上。

## 数据模型

`crates/prism-motion-matching` 分四块，互相之间只通过值类型耦合：

| 类型 | 作用 |
|---|---|
| `Trajectory` / `TrajectoryPoint` | 角色被要求去往哪里：若干未来采样点的位置、速度、朝向 |
| `PoseFeature` | 姿态签名：被追踪关节的位置与速度（`from_matrices` 用前一帧做差分） |
| `MotionDatabase` / `MotionFrame` | 每个剪辑按 `sample_interval` 采样成帧，每帧同时带姿态特征与本帧轨迹 |
| `search` / `MotionMatch` | 线性扫描，按权重把两半代价相加取最小 |

## 代价

```text
pose_cost       = 平均每关节 (|Δ位置|² + |Δ速度|²)
trajectory_cost = 平均每采样 (w_pos·|Δ位置|² + w_vel·|Δ速度|² + w_facing·(1 - dot))
cost            = w_pose·pose_cost + w_traj·trajectory_cost
```

- 关节列表长度或轨迹采样数不一致的帧**直接跳过**，不会拿错位的特征比较。
- 朝向只比水平分量：法线朝上 / 退化向量的采样按 0 计。
- `Trajectory::predicted` 是「保持当前速度直线前进」的预测，用来把玩家的输入意图
  变成查询；它不读剪辑。

## 建模约定

- 轨迹来自剪辑里**根关节**自己的未来运动（`DatabaseSettings::root_joint` 默认 0）：
  循环剪辑会 wrap，一次性剪辑在末尾保持最后一帧。
- 姿态从骨架 rest pose 开始采样单条剪辑，所以帧描述的是剪辑本身，不含实体 Transform、
  也不含混合 / 状态机输出。
- `DatabaseSettings::tracking_all` 追踪全部关节，适合先跑通；项目随后可以挑一组
  关节（左右脚、骨盆、手）缩小特征维度。

## 还没做

- 没有接进 `Animator`：查询、切帧、跨剪辑混合的运行时接线是下一片。
- 没有加速结构：几帧到几千帧的线性扫描足够，KD-tree / VP-tree 等被 profiling 要求再说。
- 没有 foot locking / 惯性化（inertialization）混合，也没有「成本超过阈值才切」的滞后策略。
