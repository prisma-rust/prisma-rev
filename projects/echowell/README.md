# EchoWell

`EchoWell` 是按需求文档制作的 2D 灰盒平台跳跃切片，目前包含 320×180 的“回声之井”和“蓄水池”两间相连房间。角色使用固定 60 Hz 模拟，包含土狼时间、跳跃输入缓冲、可变跳跃高度、二段跳、空中冲刺、能力水晶、能力门、尖刺死亡和 0.5 秒重生。

玩法代码在用户项目侧，直接复用 PrismaRev 已有的 `SpriteRenderer`、`SpriteAnimation`、`Tilemap`、`TilemapCollider2D`、`ParticleEmitter2D`、`Light2D`、正交相机、输入快照、UI quad 和存档接口。战斗使用引擎的 `Hitbox2D` / `Hurtbox2D` 与 `GameWorld::damage_contacts2d`：玩家冲刺时启用独立攻击盒，敌人使用受击盒，普通身体接触仍会造成死亡。`assets/player.json` 是可热替换的移动参数；`assets/rooms.json` 维护房间 ID、边界、瓦片、出口目标、落点和能力条件，地形碰撞由 Tilemap 的实心 tile id 自动生成，避免再维护一份 `solids` 几何。冲刺水晶解锁后可穿过井室出口，房间切换保留速度与永久能力；当前房间 ID 写入存档，死亡后回到当前房间入口。`assets/scenes/well.scene.json` 保存基础相机，房间实体由玩法系统按稳定名称生成，便于后续接 Tiled / Aseprite cooker。

碰撞约定：玩家、尖刺、能力门和水晶都以碰撞 AABB 的中心作为实体位置，使用中心 pivot，并把精灵 quad 缩放到对应碰撞尺寸；预置 scene 实体也会在运行时套用这套对齐规则。Tilemap 的 `one_way_tile_ids` 生成只从上方阻挡的单向平台，按住 `S` 或下方向键并按 `Space` 可下穿。

能力解锁写入引擎全局 flag（例如 `echowell.ability.dash`），旧版
`echowell.abilities` bit 存档会在首次启动时迁移。

## 运行

```powershell
cd projects/echowell
.\scripts\run.ps1
```

首次运行会生成原创灰盒精灵并从场景清单烘焙资源包。手动改过精灵后可用 `-SkipAssetGeneration` 保留修改；只改代码可用 `-SkipSceneBuild`。

编辑器预览模块：

```text
cargo build --no-default-features --features preview-module
```

操作：`A/D` 或方向键移动，`Space` 从一开始就能跳跃，`Shift` 在触碰金色水晶后解锁冲刺。跳过地面尖刺，到右侧紫色门前在空中冲刺穿过。`F1` 切换碰撞调试叠加层；按 `E` 开始或结束录制，按 `Enter` 从录制起点重放。

房间出口由 `rooms.json` 的 `exits` 定义；出口可声明 `required_ability`，目标房间的 `entry` 是切换后的玩家落点。相机按当前房间 `bounds` 夹取，超出视口的房间会跟随玩家滚动。

## 2D issue 对照

本切片直接验证已合并的 Tilemap（#529）、TilemapCollider2D（#529 Phase 2）、输入快照（#589）、多帧精灵动画（#584）、2D 粒子（#594）、精灵混合/排序（#610）和 Light2D（#611），并接入引擎新增的 AABB `Hitbox2D` / `Hurtbox2D` 查询。图集 cooker（#1158）、Aseprite/Tiled 离线导入（#1159）以及更完整的 Rapier2D 形状碰撞（#1160）保留为下一阶段增量；2D 虚拟相机、边界限制、插值和轻战斗接触查询已经由引擎模块提供。
