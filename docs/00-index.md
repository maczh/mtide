# mtide 文档索引

## 阅读顺序

1. [产品与功能需求](01-product-requirements.md)：明确兼容范围、优先级和验收标准。
2. [总体架构](02-architecture.md)：理解事件循环、分层、线程边界和渲染管线。
3. [模块设计与 Rust 映射](03-module-design.md)：把 Go 包映射到 Rust crate/module。
4. [领域模型与状态](04-domain-model.md)：实现工作区、文档、编辑器、Git、终端等状态机。
5. 根据任务阅读 [CLI 与配置](05-cli-config.md)、[编辑器内核](06-editor-core.md)、[服务与渲染](07-services-and-rendering.md)、[插件与 Agent](08-plugin-and-agent.md)、[Git 远程凭据交互](15-git-credentials.md)。
6. 开发前后遵守 [测试](09-testing.md) 与 [开发流程](11-development-workflow.md)。
7. 每次同步上游前阅读 [上游功能与社区建议研究](13-upstream-research.md)。
8. 开发过程中用 [当前实现状态](14-implementation-status.md) 区分已落地能力和规划能力。

## 文档状态

| 文档 | 状态 | 用途 |
|---|---|---|
| 产品需求 | 基线 | 兼容性范围与验收 |
| 架构设计 | 基线 | 模块边界与运行时约束 |
| 领域模型 | 基线 | 核心状态和不变量 |
| Rust 迁移计划 | 待执行 | 分阶段交付顺序 |
| 上游研究 | 2026-11-04 基线 | 已发布功能与社区建议 |
| 当前实现状态 | 持续更新 | 代码真实能力与下一阶段顺序 |
| API 细节 | 以实现为准 | 实现后补充精确签名与协议样例 |

## 兼容原则

- 命令 ID、快捷键语义、配置键名、`.ttt` 文件格式和 CLI 行为优先兼容。
- 终端显示宽度、Unicode 光标位置、撤销语义和异步事件顺序属于高风险兼容点。
- 若 Rust 生态导致实现差异，先记录 ADR，再修改需求或增加兼容层。
