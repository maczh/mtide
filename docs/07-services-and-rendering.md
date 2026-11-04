# LSP、Git、终端与渲染

## 1. LSP

传输是 stdio JSON-RPC。Manager 按语言/工作区懒启动 server，完成 initialize/initialized 后服务请求。文档同步采用递增版本的 full `didChange`，保存成功后发送标准 `textDocument/didSave`，标签关闭或应用退出前发送 `textDocument/didClose` 并清理文档版本；异步结果通过 event loop 返回，并校验 server/session/document version。Workspace Edit 支持当前文档内存应用，并将其他文件的 `changes`/`documentChanges` 安全写回磁盘。

首批实现顺序：diagnostics → completion/resolve → hover/signature → definition/implementation/type definition/references → rename → formatting/code actions。当前已接入 diagnostics、completion、hover、definition/implementation/type definition/references 的位置跳转与多位置选择弹层、基础 rename、formatting 和首个 Code Action；缺失能力显示 unavailable，不阻塞编辑。
当前光标行若落在诊断的多行 range 内，状态栏会显示第一条诊断消息，行号区同时保留严重级别标记。
JSON-RPC request 等待响应最多 5 秒；超时会发送 `$/cancelRequest` 并返回可显示的 LSP 错误，迟到的旧响应不会作为当前请求结果使用。

## 2. Git

Git backend 可先通过 `git` 子进程实现。需要解析 porcelain status、diff hunks、branch、log、blame；命令输出与 stderr 分离，cwd 固定为 repository root。当前已实现 status/diff/branch/blame、stage/unstage 和带非空消息校验的 commit；所有写操作支持失败通知，多仓库操作不可串错仓库。

Unified diff parser 必须接受 `@@ -4 +4 @@` 这类省略 count=1 的 hunk header。gutter 默认比较 index 与 working tree，不能错误地拿 HEAD 代替 index。VCS service 以 trait 抽象，Git-specific status 映射在 adapter 内。

工作树 diff 使用只读 overlay；working-tree diff 绑定原 editor buffer，可编辑且 debounce 重算。staged、unstaged、untracked、renamed 等状态必须保留。

当前实现保存文件后立即重新读取 Git status 和选中变更 diff，避免 Changes 面板停留在保存前状态；Changes 面板还提供受控的 commit、log、fetch、pull 和 push 操作，远程操作关闭交互式凭据提示并将失败反馈到状态栏。冲突状态会被明确标记，`o`/`t` 可分别检出 ours/theirs，随后用 `s` 暂存解决结果。仅已暂存修改会自动使用 `git diff --cached` 展示 staged diff。

## 3. 搜索与 watcher

搜索任务使用 debounce + generation + cancellation；ripgrep 输出按文件/行/列解析，路径不可直接信任，需映射回 workspace root。watcher 事件要合并短时间重复通知，并避免把自身保存误报为冲突。

## 4. PTY 与 VT

PTY session 负责 spawn、resize、write bytes、read bytes、关闭。Rust 当前已接入 portable-pty，维持多个独立持久 shell tab、后台 reader channel、输入写入和 resize；Terminal 焦点下 `Ctrl-Shift-T` 创建 tab，`Ctrl-Alt-Left/Right` 切换 tab。Close 必须幂等且只执行一次，能处理命令退出后仍存活的 background jobs。VT parser 维护 cursor、application cursor key mode、attributes、alternate screen、scrollback、OSC/链接等状态，并按单元记录 OSC 8 超链接。终端未启用鼠标协议时，点击 `http://` 或 `https://` 链接可调用系统打开器；非 HTTP(S) 协议会被拒绝，启用终端鼠标协议的应用则优先接管鼠标事件。终端色彩允许直接 RGB；其他 widget 使用语义主题色。Ctrl+V 是否粘贴由配置决定。

终端 focus 时实现 `RawKeyConsumer` 语义：一般输入转成 VT bytes；force keys（至少 terminal.toggle）优先由应用处理。PTY 输出事件唤醒主循环。

## 5. 渲染

Cell 至少包含 glyph、foreground、background、attributes、width/continuation。current/previous 双缓冲比较后只写差异。所有 panel 使用统一 layout rect；draw 不得改变业务状态。resize 后先重排再重绘，避免使用旧坐标。
