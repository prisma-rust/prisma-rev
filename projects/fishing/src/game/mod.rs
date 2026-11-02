//! 玩法模块（用户项目侧）。
//!
//! 这里只放**钓鱼游戏自己的东西**：船、鱼线、鱼种数据、HUD、状态机。
//! 引擎 crate 不知道任何钓鱼概念（见
//! `projects/fishing/DESIGN.md` §2 分层契约）。

pub mod boat;
pub mod fishing;
pub mod world;
