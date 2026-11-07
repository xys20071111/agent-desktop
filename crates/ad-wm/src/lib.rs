//! ad-wm：窗口模型、布局算法、输入映射与截图编码。
//!
//! 与 smithay 强耦合的部分（Space/element 接线）位于 agentdesk crate；
//! 本 crate 承载可独立测试的纯逻辑与模型类型。

pub mod keymap;
pub mod layout;
