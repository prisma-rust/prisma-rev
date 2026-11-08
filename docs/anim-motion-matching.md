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
| `SelectionHistory` / `SearchBias` | 最近的选择，用来让决定「粘住」而不是在两个几乎同价的帧之间跳 |

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

## 持续与重选

纯最近邻会在两个只差几毫米的帧之间来回跳，看起来像卡顿：同一条走路的相邻采样代价几乎
相同，赢家每几帧就翻一次。真实动作匹配用两个手段让决定稳定，这里都表达成**代价乘数**
（`SearchBias`），不改扫描逻辑：

```text
cost = (w_pose·pose_cost + w_traj·trajectory_cost) · multiplier(frame)
multiplier = bias.continuing   若 frame 是 `SelectionHistory` 里最后选中的那一帧（默认 0.85）
           = bias.reselect     若 frame 最近被选中过但已经不是当前帧（默认 1.15）
           = 1.0               其它
```

- `search`（不带历史）等价于 `SearchBias::NONE`，行为与之前完全一致；需要粘住时用
  `search_biased(..., bias, Some(&history))`。
- 返回的 `MotionMatch` 里 `cost` 已含乘数，`pose_cost` / `trajectory_cost` 保持原始距离，
  调试面板能直接看出「保留它是因为便宜还是因为本来就在播」。
- 历史只记**决定**（`frame` / `clip` / `time`），不存姿态；重复记录同一帧只刷新时间，
  不会把窗口塞满。`clear()` 用于传送、场景重载这类「上一帧的姿态不再相关」的时机。
- 乘数只做缩放：`continuing = 1.0` / `reselect = 1.0` 就是纯粹的独立决策；非有限或
  非正数的配置按 1.0 处理，不会把代价变成负数。
- 哪一帧「明显更好」由乘数决定：`continuing = 0.5` 意味着另一帧要便宜一半才会切换。

## 建模约定

- 轨迹来自剪辑里**根关节**自己的未来运动（`DatabaseSettings::root_joint` 默认 0）：
  循环剪辑会 wrap，一次性剪辑在末尾保持最后一帧。
- 姿态从骨架 rest pose 开始采样单条剪辑，所以帧描述的是剪辑本身，不含实体 Transform、
  也不含混合 / 状态机输出。
- `DatabaseSettings::tracking_all` 追踪全部关节，适合先跑通；项目随后可以挑一组
  关节（左右脚、骨盆、手）缩小特征维度。

## 脚部锁定

换帧时新旧帧的步态相位不同，脚会瞬移几厘米，看起来像地面在脚下滑。
`FootLock` 记住「这只脚已经踩实的位置」，之后每次求值把脚拉回原位，直到它自己迈步：

```rust
let mut lock = FootLock::new(2);
let settings = FootLockSettings { joints: vec![left_foot, right_foot], ..Default::default() };
let report = lock.update(&skeleton, &mut pose, &settings, dt);
```

- **踩实 / 松脚按速度判定**：单帧速度 ≤ `plant_speed`（默认 0.35 m/s）算踩实，
  > `release_speed`（默认 0.9 m/s）立刻松开。速度用的是「上次求值后的位置」，
  锁定修正本身不计入，否则会自我抵消。
- **修正有上限**：`max_offset`（默认 0.25 m）夹住单帧位移，锁太久会把腿拉成
  长短腿；宁可松脚也不要毁掉姿态。
- `weight = 0`（或非有限）直接关闭，姿态一个字节都不改；`dt <= 0` 或非有限时
  只记录位置、不做速度判定。
- 修正量先转到父关节空间再写回局部平移，所以子关节（脚趾）跟着走。
- 换场景 / 传送后调用 `reset()`，否则脚会被拉回旧位置。

## 还没做

- 没有接进 `Animator`：查询、切帧、跨剪辑混合的运行时接线是下一片。
- 没有加速结构：几帧到几千帧的线性扫描足够，KD-tree / VP-tree 等被 profiling 要求再说。
- 没有惯性化（inertialization）混合：换帧目前还是硬切 + 脚部锁定。
- 历史上的「最近选中过」只看帧号，没有按时间衰减的权重；需要时可以在 `SearchBias`
  之外再加一层年龄权重。
