# 领域模型与状态

## 1. Workspace

```text
Workspace
 ├─ roots: Vec<WorkspaceRoot>
 ├─ repositories: Map<RepoId, Repository>
 ├─ documents: Map<DocumentId, Document>
 ├─ tabs: Vec<Tab>
 └─ active_tab / active_root
```

`WorkspaceRoot` 保存规范化路径、显示名和可选 repo。活动文件按路径最长前缀选择 root；仓库能力使用包含该文件的 git root。

Workspace 还可保存 `DefaultStartupFolders`。启动优先级为显式文件/目录/PR/`--workspace`/`--welcome` > 默认 folders > 普通启动；默认文件不存在或损坏时回退，不阻塞启动。

## 2. Document 与 Buffer

`Document` 包含 URI/path、文本缓冲、文件编码/换行风格、dirty、文件版本、外部修改状态、语言 ID 和关联 LSP 文档。`Buffer` 保存行序列和版本；编辑通过 `EditCommand` 记录 before/after、范围和 selection/cursor 快照。

状态：`Clean → Dirty → Saving → Clean`，保存失败回到 `Dirty` 并通知；外部变化为 `Clean/Dirty → ExternalChanged`，由用户选择 reload/keep/compare。`Dirty` 必须比较当前内容与 save baseline，不能简单以 undo 栈为空判断。

## 3. 光标、选区、多光标

- `Position { line, col }` 的 `col` 使用字符索引。
- `Selection { anchor, active }` 规范化为 range 时保留方向。
- 多光标编辑前按文档位置倒序应用，避免前方修改使后方偏移。剪切范围先按原始坐标归并，防止整行 cursor 与 selection 重叠时误删。
- 复制、删除、格式化和 LSP range 转换必须明确 byte/rune/visual 三种坐标。

## 4. Undo/Redo

Undo stack 存放语义 command，不存完整文件快照作为默认方案。连续输入可按时间、光标连续性和操作类型合并；批量多光标编辑是一个用户可感知步骤。Redo 在新 mutation 后清空。

## 5. Fold 与 Diff

Fold 是 view/document presentation state，不应修改原文。Diff overlay 是 editor view 的行映射层，包含 added/deleted/unchanged/filler/gap；changes-only 的隐藏行与用户 fold 分离。

额外视图状态包括 bookmarks、手动 language override、horizontal scroll、soft-wrap column 和 merge-conflict block selection。

## 6. LSP 状态

每个语言/工作区服务：`Stopped → Starting → Initializing → Ready → Failed → Stopping`。请求关联 document version；旧诊断、completion、hover 结果不得覆盖新文档。

## 7. Terminal session

`TerminalSession { id, command, cwd, pty, vt_parser, screen_grid, scrollback, status }`。PTY 输出只追加 bytes，VT parser 生成 cell mutation；关闭状态必须可区分正常退出、信号退出和启动失败。

## 8. UI 状态

Root UI 管理 focus、overlay stack、panel visibility、active group、menu/dialog、status segments。focus 变化是显式状态，不依赖“最后一次绘制的 widget”。
