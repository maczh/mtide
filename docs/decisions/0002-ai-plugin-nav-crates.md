# ADR 0002: 建立 AI、插件与导航独立契约层

## 状态

已接受。

## 决策

按架构设计文档建立 `mtide-ai`、`mtide-plugin`、`mtide-nav` 三个独立 crate。

- `mtide-ai` 只负责适配器契约、事件解析、上下文外发过滤、Diff Gate 基线校验、会话元数据和审计 DTO；它不直接写工作区文件。
- `mtide-plugin` 只负责清单、权限 broker、stdio JSON-RPC、生命周期和 ANSI-free Widget 树；它不依赖 Git 或 AI。
- `mtide-nav` 负责最近文件、位置、书签、本地快照和符号大纲状态；UI 由二进制层渲染。

所有新增功能先在无终端单元测试中验证，再由 `mtide` 编排；自动安装、网络下载和 AI 写盘仍需显式确认或 Diff Gate。

## 取舍

本次先交付纯契约与安全边界，避免把尚未实现的 LSP、网络安装或 CLI 交互伪装成完成状态。后续 UI 接入可保持依赖方向和协议稳定。
