# 模块设计与 Rust 映射

建议先使用单 workspace、多 crate 结构，避免过早拆成网络式微服务。

| Rust crate/module | 对应 ttt | 职责 |
|---|---|---|
| `mtide-cli` | `cmd/ttt` | 参数、启动、panic/crash、版本 |
| `mtide-core` | `internal/core` | Buffer、Cursor、Selection、Undo、Fold、Diff、MultiCursor |
| `mtide-workspace` | `internal/workspace` | roots、documents、tabs、`.ttt` |
| `mtide-config` | `internal/config` | settings、keybindings、theme、editorconfig |
| `mtide-command` | `internal/command` | command ID、注册、执行、参数 |
| `mtide-terminal` | `internal/term`, `terminal` | screen、cell、PTY、VT、宽度 |
| `mtide-ui` | `internal/ui`, `widgets`, `view` | layout、focus、overlay、panel、组件 |
| `mtide-render` | `internal/render` | double buffer、cell diff、flush |
| `mtide-lsp` | `internal/lsp`, `jsonrpc` | JSON-RPC、stdio LSP、生命周期 |
| `mtide-git` | `internal/git`, `github` | Git 命令、diff、PR API |
| `mtide-search` | `internal/search` | ripgrep、结果模型、去抖 |
| `mtide-plugin` | `internal/plugin` | Lua runtime、manifest、permissions、Widget API |
| `mtide-agent` | `internal/ai` | Agent 配置、PTY 会话、安装命令 |
| `mtide-markdown` | `internal/markdown` | Markdown → styled lines |
| `mtide-vcs` | `internal/git` 的抽象层 | Git/JJ backend、统一 status/diff/history 能力 |

## 依赖方向

`core ← workspace ← services ← app ← ui/cli`。`core` 只能依赖标准库和纯算法 crate；`ui` 可依赖所有上层 service trait，但 service 不得依赖具体 widget。

## Trait 边界

- `FileSystem`: read/write/rename/remove/list/metadata。
- `ProcessRunner`: 启动命令、读写 stdin/stdout、取消。
- `GitBackend`: status/stage/commit/diff/history/branch/remote。
- `LanguageServerTransport`: JSON-RPC request/notification/close。
- `TerminalBackend`: PTY、screen、cursor、mouse、clipboard。
- `Clock` / `Scheduler`: 测试中替换真实时间和 timer。
- `PluginHost`: load/unload/call/register。
- `VcsBackend`: status/stage/commit/diff/history/branch，Git 与 jj 通过 adapter 实现。
- `Clipboard`: text/image read/write，供 editor 和 Markdown resource paste 使用。

## 错误处理

使用分层错误：底层 `thiserror` 风格的结构化错误，上层转换为 `AppError` 并产生用户通知/Output 日志。错误不能只写日志后静默吞掉；每个外部操作都要定义失败后的 UI 状态。

## 命名与稳定 API

命令采用 `domain.verbNoun`，例如 `editor.joinLines`、`fold.toggle`、`multicursor.selectAll`。配置 key、命令 ID、插件公开函数和工作区格式视为兼容 API，变更需记录迁移策略。
