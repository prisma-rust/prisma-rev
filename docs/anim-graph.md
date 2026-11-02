# 动画图格式（RAGP v1，#560 P1）

动画图把剪辑、混合空间、BlendStack、Slot、分层混合和状态机连成一张有向无环图。
作者写 `*.animgraph.json`，cooker 校验后编成 `.rag`（RAGP v1）放进 `.pak`；
运行时只解码，不在加载时查名字。

> 当前范围：格式、作者 JSON 校验、cooker 编码、构建管线导入、运行时解码、资源类型
> 和 `AnimGraphEvaluator`。把图自动接进引擎 `Animator` 仍是后续步骤。

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
    input_count:4 × { node:u32, weight:f32, mask_count:4 + names }
}
[output:u32]
```

`flags`：bit0 = looped，bit1 = additive。所有计数都有上限；截断、非法 magic /
version、未知 kind / flags、越界节点下标和尾随字节都会让解码返回 `None`。

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
- `Slot` / `CachedPose` / `Output` 当前是透明传递；Slot 的 Montage 队列是后续切片。
- StateMachine 节点内部维护当前状态、状态时间和转移交叉淡入；Trigger 按状态机语义消耗。
- 剪辑 / 状态机缺失、空混合空间、空状态机、成环都返回 `GraphEvalError`，不会 panic。
