# 测试与质量门禁

## 1. 测试分层

| 层级 | 内容 | 目标 |
|---|---|---|
| Unit | buffer、cursor、selection、undo、fold、diff、parser、config | 纯算法和边界 |
| Component | widget、layout、key matching、render grid | UI 组件组合 |
| E2E | in-memory screen + 完整 App | 用户操作与布局 |
| Functional | 编译后的二进制 + `--exec` | 启动、命令、文件效果、可见结果 |
| Integration | real PTY、LSP、watcher、外部 Git | 真实进程边界 |
| Benchmark/Chaos | 大文件、并发异步、PTY/LSP 异常 | 性能与恢复性 |

## 2. 必测契约

- UTF-8、全角、组合字符、tab 的 cursor/visual mapping。
- 插入、删除、多光标、折叠、undo/redo 的状态恢复。
- save/reload/external-change 冲突。
- 搜索去抖和旧结果丢弃。
- LSP server 启停、超时、坏 JSON、旧版本诊断。
- Git staged/unstaged/untracked/diff 更新。
- PTY raw key、resize、VT colors、终端退出。
- `--exec` 动作错误码、`wait-for`、debug/screenshot。
- plugin permission denial 和崩溃隔离。
- dirty baseline 与 undo/redo 往返，确保未保存内容不被错误丢弃。
- 多光标复制/剪切/粘贴的顺序、整行 fallback、spread、重叠范围和单步 undo。
- soft wrap 的 screen-row 导航、horizontal scroll、auto-closing、语言 comment syntax 和 language override。
- focus overlay 恢复、Find/Replace 切换、搜索框/终端/插件输入的粘贴路由。
- diff hunk 缺省 count=1、确定性 indent tie-break、merge conflict block 和 jj adapter contract。

## 3. Rust 质量门禁

提交前运行格式化、clippy、单元测试、文档测试和受影响的集成测试。CI 再运行全量测试、多平台构建和 sanitizer/benchmark（适用时）。任何 flaky 测试必须记录原因，不得用无意义 sleep 掩盖竞态；等待应基于可观察状态。

## 4. 测试替身

所有时间、文件系统、进程、终端屏幕、剪贴板、网络和 LSP transport 都应可替换。测试需要确定性事件队列，能手动推进 timer 和异步消息。

## 5. 可观测性

debug 模式记录启动、命令、LSP、Git、插件和 PTY 生命周期；Output panel 与日志共享事件源，但不能把敏感数据写入日志。崩溃报告包含版本、平台、stack 和最近事件摘要。
