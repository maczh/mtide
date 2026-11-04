# mtide 开发文档

`mtide` 是对 Go 项目 `ttt`（Terminal Text Tool）的 Rust 重构项目。

当前仓库已完成第一阶段可运行实现：包含响应式 TUI 布局、编辑器核心、PTY 终端、Git 工作区与分支操作、LSP 基础能力、Lua 插件运行时和多提供商 Agent 面板。完整的功能边界、已实现项与后续工作见[实现状态](docs/14-implementation-status.md)。

## 文档入口

- [文档索引](docs/00-index.md)
- [产品与功能需求](docs/01-product-requirements.md)
- [总体架构](docs/02-architecture.md)
- [模块设计与 Rust 映射](docs/03-module-design.md)
- [领域模型与状态](docs/04-domain-model.md)
- [CLI、配置与工作区文件](docs/05-cli-config.md)
- [编辑器内核](docs/06-editor-core.md)
- [LSP、Git、终端与渲染](docs/07-services-and-rendering.md)
- [插件与 AI Agent](docs/08-plugin-and-agent.md)
- [测试与质量门禁](docs/09-testing.md)
- [Rust 重构路线图](docs/10-rust-migration-plan.md)
- [开发流程](docs/11-development-workflow.md)
- [术语表](docs/12-glossary.md)
- [上游功能与社区建议研究](docs/13-upstream-research.md)
- [实现状态与后续工作](docs/14-implementation-status.md)

## 事实来源

文档基线来自 `~/Work/go/src/github.com/maczh/ttt/` 的源码、`README.md`、`PRD.md`、`ARCHITECTURE.md`、`docs-web`、配置样例和测试目录。原项目的行为是兼容目标；Rust 实现可以替换内部技术，但不得无明确决策地改变用户可见行为。
