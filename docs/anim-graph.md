# 动画图格式（RAGP v2，#560 P1）

动画图把剪辑、混合空间、BlendStack、Slot、分层混合和状态机连成一张有向无环图。
作者写 `*.animgraph.json`，cooker 校验后编成 `.rag`（RAGP v2）放进 `.pak`；
运行时只解码，不在加载时查名字。

> 当前范围：格式、作者 JSON 校验、cooker 编码、构建管线导入、运行时解码、资源类型、
> `AnimGraphEvaluator`，以及 `Animator.anim_graph_path` 的运行时驱动。

## 作者格式

```json
{
  "parameters": [
    { "name": "speed", "type": "float", "default": 0.0 },
    { "name": "aiming", "type": "bool", "default": false }
  ],
  "nodes": [
    {
      "name": "locomotion",
      "type": "blend_space_1d",
      "clips": ["idle.ranim", "run.ranim"],
      "positions": [0.0, 5.0],
      "speeds": [1.0, 2.0],
      "parameter": "speed"
    },
    {
      "name": "aim",
      "type": "clip_player",
      "clip": "aim.ranim",
      "additive": true,
      "weight": 0.5,
      "bone_mask": ["spine_02", "hand_r"]
    },
    {
      "name": "stack",
      "type": "blend_stack",
      "inputs": ["locomotion", { "node": "aim", "weight": 0.5 }]
    },
    { "name": "out", "type": "output", "input": "stack" }
  ]
}
```

规则：

- `type`：`clip_player`、`blend_space_1d`、`blend_space_2d`、`blend_stack`、
  `slot`、`layered_blend`、`cached_pose`、`state_machine`、`output`。
- 节点名必须非空且唯一；`inputs` 里的字符串等价于权重 1 的连线，也可以写
  `{ "node": "aim", "weight": 0.5, "bone_mask": ["spine_02"] }`。
- `blend_space_1d` 至少 2 个剪辑，`positions` 与剪辑一一对应；
  `blend_space_2d` 至少 3 个剪辑，`positions` 是 `[x, y]`。
- 可选 `speeds` 给 BlendSpace 每个样本独立播放速度，长度必须等于剪辑数，
  值有限且 `>= 0`；缺省 / 全 1 时使用共享相位。
- `slot` / `cached_pose` / `output` 恰好 1 个输入；
  `blend_stack` / `layered_blend` 至少 1 个输入。
- 图必须恰好有 1 个 `output` 节点；连线必须指向已存在的节点，且不能成环。
- 权重被夹到 `0..=1`，非有限值回落到默认值；旧工具缺省时 `speed = 1`、
  `looped = true`、`weight = 1`、`additive = false`。

## 运行时接口

- `prism_asset::runtime::AnimGraphAsset` —— `ResourceManager` 类型化加载 `.rag`。
- `prism_asset::formats_anim_graph::AnimGraphInfo` —— 参数、节点、输入下标和输出下标。
- `AssetType::AnimGraph` / 扩展名 `rag`。

## 构建管线

- `prism-cook import` / `build` 通过 `AnimGraphImporter` 识别 `*.animgraph.json`：
  解析并校验作者 JSON，编码成 RAGP 中间数据，同时把剪辑 / 状态机路径解析成资产依赖。
- `AnimGraphCooker` 校验中间数据的 RAGP magic / 版本 / 结构，再原样打包进 `.pak`
  （`AssetType::AnimGraph`）。
- 导入缓存 / 依赖拓扑排序复用现有资源管线；图格式非法会让该资产导入失败，而不是
  把坏数据带进包。

## 字节布局

```text
[magic:4 "RAGP"][version:1]
[parameter_count:4]
parameters: count × { name:u16+utf8, kind:u8, default:f32 }
[node_count:4]
nodes: count × {
    name:u16+utf8, kind:u8, flags:u8, speed:f32, weight:f32,
    clip_count:4 + clips, sample_count:4 + [x,y] f32,
    param_x:u16+utf8, param_y:u16+utf8, machine:u16+utf8,
    input_count:4 × { node:u32, weight:f32, mask_count:4 + names },
    speed_count:4 + f32 × count          // v2；v1 无此表
}
[output:u32]
```

`flags`：bit0 = looped，bit1 = additive。所有计数都有上限；截断、非法 magic /
version、未知 kind / flags、越界节点下标和尾随字节都会让解码返回 `None`。
v1 剪辑（无速度表）仍可解码，`sample_speeds` 为空。

## 运行时求值

`prism_anim::AnimGraphEvaluator` 把 `AnimGraphInfo` 求成 `Pose`：

```rust
struct Assets { /* 已解码的 Clip / StateMachine */ }
impl AnimGraphSource for Assets {
    fn clip(&self, path: &str) -> Option<Arc<Clip>> { /* ... */ }
    fn state_machine(&self, path: &str) -> Option<Arc<StateMachine>> { /* ... */ }
}

let mut evaluator = AnimGraphEvaluator::new(graph_info);
evaluator.parameters_mut().set_float("speed", 4.0);
let pose = evaluator.evaluate(&assets, &skeleton, dt)?;
```

- 每个节点每帧只求值一次；ClipPlayer / BlendSpace / StateMachine 各自推进时间。
- `BlendStack` / `LayeredBlend` 的第一个输入是 base，其余输入按 weight 与骨骼遮罩叠加。
- `Slot` 在基础输入上按 `weight` 覆盖一段可选 montage 剪辑（`clips[0]`，独立时间/循环/Notify）；没有剪辑时透明传递；求值器也为每个 Slot 维护 Montage 播放队列（见下）。`CachedPose` / `Output` 是透明传递。
- StateMachine 节点内部维护当前状态、状态时间和转移交叉淡入；Trigger 按状态机语义消耗。
- `evaluate_with_events` 额外返回 `GraphNotifyEvent`：ClipPlayer / StateMachine 跨过
  剪辑 Notify 时触发，循环回绕分两段扫描，第一帧不补发 time 0，共享节点只发一次。
  BlendSpace 默认只让**权重最大的样本**产生事件（`BlendSpaceNotifyPolicy::Dominant`），
  避免多个采样点重复脚步声；切到 `BlendSpaceNotifyPolicy::None` 可完全关闭。
  状态机默认在转移帧仍扫描离开状态到转移点为止；`StateTransitionNotifyPolicy::SuppressOnTransition`
  会丢掉这一帧离开状态的事件，避免“刚抬脚就切走”的脚步声。
- 剪辑 / 状态机缺失、空混合空间、空状态机、成环都返回 `GraphEvalError`，不会 panic。

Montage 播放由 `prism_anim::MontagePlayer` 负责：`play_now` 立即覆盖、`enqueue`
FIFO 排队、单次播完自动接下一段，`stop` / `stop_after_current` 清空对应状态，
`tick` 收集跨帧 Notify。求值器把它接到 Slot 节点：`play_montage` /
`enqueue_montage` / `stop_montage` 按节点名操作，活动段按权重覆盖 Slot 输入；
没有活动段时回退到作者写的 `clips[0]`。

同步标记：`.ranim` v3 的同步标记表通过 `Clip::sync_marker_set()` 转成
`SyncMarkerSet`；资产侧来自 `*.skinning/sync_markers.json`。`BlendSpace1D/2D`
在所有样本都有同结构标记时自动建 `SyncGroup`，以**当前权重最大的样本**为领跑剪辑把
相位映射到其余样本（参数切换时 leader 跟着换）；标记缺失或数量不一致时退回共用归一化相位。
求值器可用 `set_blend_space_speeds(node, speeds)` 给样本配置独立播放头（按 `node.clips` 顺序），
此时每个样本按自己的速度推进、跳过 SyncGroup 映射；空配置恢复共享相位。

编辑器面板：`Window → General → Animation Graph` 选中 `.animgraph.json` / `.statemachine.json`
后显示结构化视图（参数、节点、连线、状态、转移条件）；面板顶部的 `↶ Undo edit` / `↷ Redo edit`
撤销 / 重做该资产的 JSON 改动（整份快照、连续拖拽合并成一步；参数改名连带状态机的多文件改动
是同一组、一起回滚，和场景撤销栈相互独立）。
面板顶部会列出 cooker 会拒绝的形状问题（节点类型 / 剪辑数 / 连线 / 输出 / 环 / 参数），
图侧可以增删参数、改默认值、重命名参数
（同步改写混合空间引用与图引用状态机侧车的条件名；被引用的参数不能删）、拖动布局、增删 / 重命名 / 复制剪辑节点、
改节点的剪辑 / speed / 权重 / 循环、改混合空间参数名 / 样本坐标（含二维画布拖点，并提示共线退化）/ 样本增删、增删连线并改权重 /
骨骼掩码；状态机侧可以改状态的名字 / 剪辑 / speed / looped、复制状态、增删转移、重连转移的 `from` / `to`、
用 `↑` / `↓` 调整转移顺序（顺序即优先级，运行时取第一条命中的转移）、拖转移的 `blend` / `min` 时长，以及增删条件和改 float 阈值 / bool 期望值。选中
`*.skinning/notifies.json` 时同一个面板变成事件表编辑器：按剪辑列出 Notify，可改时间 / 名字、
删除，或用草稿表单追加（包括新建剪辑键）；每个剪辑的时间轴可以直接拖动金色标记改时间，空白处点击会用该时间打开新增草稿。

`Animator.blend_smoothing`（秒）开启后，混合空间参数按指数平滑趋近目标，避免速度跳变
让姿态弹一下；默认 0 = 直通。

运行时每帧还会在 `AnimatorPlayhead.root_motion` 记录根关节相对上一帧的位移 / 偏航增量
（首帧与换剪辑首帧为零）。默认只记录，不改实体 Transform。

`Animator.root_motion_consume = true`（#560 P2）时，引擎把这一帧的**根平移**加到实体的
`LocalTransform.position` 上（先按实体当前朝向旋到世界空间），同时把姿态里的根平移清零，
避免「实体走了、骨骼根也跟着走」的双倍位移。首帧 / 换剪辑首帧 delta 为零，所以不会瞬移。
根**旋转**增量目前仍然只记录、不消费：偏航要连同整条骨骼链一起扣掉才不会双转，
留给 CharacterMovement 统一处理。

## 实时调试数据（#560）

调试叠加层 / 编辑器工具可以往 World 里插入一个 `AnimGraphDebug` 资源（默认不存在）：

```rust
world.insert_resource(AnimGraphDebug::default());
// 每帧 animator_system 之后：
if let Some(states) = world.get_resource::<AnimGraphDebug>().and_then(|d| d.states(entity)) {
    for node in states {
        // node.name / node.kind / node.time / node.state
        // node.blending / node.blend_weight / node.cached
    }
}
```

资源存在时引擎才每帧重建快照（节点顺序与图一致，字符串会拷贝）；不存在时零开销。
冻结 / 降频的图实体保留上一次快照，方便看到「停在哪」。`time` 是节点自己的本地时间，
状态机节点的 `state` 是当前状态下标、`blending` / `blend_weight` 反映交叉淡入，
`cached` 表示 `cached_pose` 节点是否已有缓存姿态。

引擎侧项目代码通过 `AnimMontageRequests` 资源发请求（`MontageRequest::play` /
`enqueue` / `stop` / `stop_after_current`，可带循环 / 速度 / 权重 / 开始时间）；
`animator_system` 每帧消费一次，把剪辑路径解析成 `Montage` 注入对应实体的 Slot。
请求只活一帧；剪辑未加载、实体没有图或 Slot 名不存在时忽略并告警。

引擎侧：`Animator.anim_graph_path` 非空且图已加载时优先于状态机 / 混合空间 /
单剪辑；`load_animation_assets` 会加载 `.rag` / `*.animgraph.json`，再补加载图引用的
状态机和剪辑。`animator_system` 为每个实体持久化一个求值器，把项目参数表的
float/bool/trigger 同步进去，并把图 Notify 合并进现有 `AnimEvents`；图失败时
回退到普通剪辑路径并记录警告。

多角色并行（#560 P6）：`animator_system` 先**顺序**应用 Montage 请求、同步参数，
再把每个实体自己的求值器从表里取出，交给 `prism_ecs::jobs::scoped_parallel_chunks`
按块并行求值（最多 8 个工作线程）；姿态、播放头与事件仍然回到主线程顺序写进 ECS。
求值器之间不共享状态，所以结果与单线程一致 —— 少于两个图实体或单核设备直接顺序跑，
不建线程。
