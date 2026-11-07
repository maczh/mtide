# LSP、Git、终端与渲染

## 1. LSP

传输是 stdio JSON-RPC。Manager 按语言/工作区懒启动 server，完成 initialize/initialized 后服务请求。文档同步采用递增版本的 full `didChange`，保存成功后发送标准 `textDocument/didSave`，标签关闭或应用退出前发送 `textDocument/didClose` 并清理文档版本；异步结果通过 event loop 返回，并校验 server/session/document version。Workspace Edit 支持当前文档内存应用，并将其他文件的 `changes`/`documentChanges` 安全写回磁盘。服务端反向请求会安全回包：`workspace/configuration` 返回与请求项数量匹配的空配置，`workspace/workspaceFolders` 返回当前多根工作区，动态能力注册、进度创建和消息请求返回标准空结果，避免语言服务器等待。

首批实现顺序：diagnostics → completion/resolve → hover/signature → definition/implementation/type definition/references → rename → formatting/code actions。当前已接入 diagnostics、completion、hover、definition/implementation/type definition/references 的位置跳转与多位置选择弹层、基础 rename、formatting 和首个 Code Action；completion 会优先应用 `insertText`/`textEdit.newText`，并按 `textEdit.range` 使用 UTF-16 列替换指定范围；缺失能力显示 unavailable，不阻塞编辑。
当前光标行若落在诊断的多行 range 内，状态栏会显示第一条诊断消息，行号区同时保留严重级别标记；侧栏可用 `Ctrl-K D` 打开 Problems，按 `↑/↓` 浏览当前文件的 LSP 与插件诊断，Enter 跳转到诊断位置。
状态栏同时显示当前文件的工作区相对路径；欢迎缓冲区没有路径时显示 `welcome.md`。
编辑器查询和编辑类 JSON-RPC request（completion、hover、signature help、symbols、definition、references、implementation、type definition、Rename、Formatting、Range Formatting、Code Action、`workspace/executeCommand`）由主循环异步发出并轮询响应，单请求最多等待 5 秒；超时会发送 `$/cancelRequest`，迟到的旧响应不会作为当前请求结果使用。服务初始化、文档同步仍沿用受控同步路径。服务端主动发送 `workspace/applyEdit` 时，Host 会复用同一套 URI、只读、范围重叠和资源操作预检，应用成功或失败均回复 JSON-RPC `applied`/`failureReason` 结果；`window/showMessage` 与 `window/logMessage` 会进入状态栏，不再静默丢弃。

## 2. Git

Git backend 可先通过 `git` 子进程实现。需要解析 porcelain status、diff hunks、branch、log、blame；命令输出与 stderr 分离，cwd 固定为 repository root。当前已实现 status/diff/branch/blame、stage/unstage 和带非空消息校验的 commit；所有写操作支持失败通知，多仓库操作不可串错仓库。

Unified diff parser 必须接受 `@@ -4 +4 @@` 这类省略 count=1 的 hunk header。gutter 默认比较 index 与 working tree，不能错误地拿 HEAD 代替 index。VCS service 以 `VcsBackend` trait 抽象，当前由 `GitBackend` adapter 提供 status/diff 能力并接入工作区状态读取；Git-specific 命令映射留在 adapter 内，为后续 `jj` 等后端保留替换边界。

工作树 diff 使用只读 overlay；working-tree diff 绑定原 editor buffer，可编辑且 debounce 重算。staged、unstaged、untracked、renamed 等状态必须保留。

当前实现保存文件后立即重新读取 Git status 和选中变更 diff，避免 Changes 面板停留在保存前状态；Changes 面板还提供受控的 commit、log、fetch、pull 和 push 操作。远程操作在后台线程执行，结果由主循环接收；默认关闭交互式凭据提示，认证失败时由 TUI 提供内存凭据 overlay 和一次性重试。冲突状态会被明确标记，`o`/`t` 可分别检出 ours/theirs，随后用 `s` 暂存解决结果。仅已暂存修改会自动使用 `git diff --cached` 展示 staged diff。Output diff 默认使用 changes-only 视图，将超过 6 行的安静上下文折叠为可操作的 disclosure row；`Enter`/`e` 在当前行展开该 gap，`c` 在 changes-only/full context 间切换，`v` 切换 unified/split 视图。

Changes 侧栏提供持久化的 `git.fileView` 列表/树视图，默认按 `v` 切换并立即保存。两种视图都分为 Staged Changes 和 Changes；同时包含暂存与工作树修改的文件会在两个分区分别出现，并保留 index/worktree 双状态。树视图按工作区根和目录生成可折叠层级，Up/Down 导航，Right/Left 展开、折叠或进入父级，Enter 切换分区/目录；点击文件切换当前 diff，点击分区或目录折叠/展开。目录和分区行的 `s`、`u`、Space 可批量 stage、unstage 或按当前状态切换对应变更。

Git History 以固定 HEAD 快照每页读取 10 条记录，`]` 加载更早提交；选中提交按 Enter 可异步打开完整提交元数据与补丁。详情内 `v` 切换 split/unified，`c` 切换变更行/完整上下文并后台重载，Esc/Backspace 返回 history。详情补丁中的 Ctrl-click 文件引用按当前 diff 文件路径解析。PR Review 中按 `m` 在 diff 与评论列表间切换，评论列表按 Enter 跳转到文件位置；在 diff 视图按 `c` 撰写评论，评论正文 Enter 提交、Esc 取消。`editor.diffWordWrap` 与 `editor.diffHighContrast` 是跨 Changes、提交详情和 PR diff 的共享呈现设置；设置面板可切换两项，命令面板提供 `options.toggleDiffWordWrap`、`options.toggleDiffHighContrast`，以及选择 changes-only/full context、split/unified 的命令。命令切换立即保存配置。

## 3. 搜索与 watcher

Git 远程操作还提供 `sync`：后台执行 `pull --ff-only`，仅在 pull 成功后执行 push；认证重试沿用同一内存凭据 overlay。

搜索任务使用 debounce + generation + cancellation；ripgrep 输出按文件/行/列解析，路径不可直接信任，需映射回 workspace root。watcher 事件要合并短时间重复通知，并避免把自身保存误报为冲突。

## 4. PTY 与 VT

PTY session 负责 spawn、resize、write bytes、read bytes、关闭。Rust 当前已接入 portable-pty，维持多个独立持久 shell tab、后台 reader channel、输入写入和 resize；Terminal 焦点下 `Ctrl-Shift-T` 创建 tab，`Ctrl-Alt-Left/Right` 切换 tab。Close 必须幂等且只执行一次，能处理命令退出后仍存活的 background jobs。VT parser 维护 cursor、application cursor/keypad mode、attributes、alternate screen（`?47` 保留备用缓冲、`?1047` 退出时清除、`?1049` 进入时清屏并恢复主屏光标）、scrollback、OSC/链接等状态，并按单元记录 OSC 8 超链接；主屏 scrollback 在 resize 缩窄时按新列宽重排，避免长输出历史行被截断。Terminal 与独立 Agent PTY 在 alternate screen 下均支持 Escape、F1–F12、Home/End、Insert/Delete、PageUp/PageDown VT 序列，方向键继续尊重各自 application cursor mode。OSC 133 A/B/C/D shell prompt mark 已接入，非 alternate-screen resize 前会清理标记范围内的旧 prompt，兼容 fish 等会在 SIGWINCH 后重绘提示符的 shell。DECRQM `CSI ?Ps$p` 支持批量模式查询并逐项返回状态；`CSI ! p` soft reset 会保留屏幕内容，同时清理光标/属性模式、鼠标编码、焦点上报、滚动区域和自定义 Tab stop；Xterm `CSI ? Ps s/r` 可按模式分别保存/恢复已支持的 DEC 私有模式，普通 `CSI s/u` 仍用于光标状态。终端焦点下 Alt 字符、Shift-Tab 及带修饰键的光标/分页/功能键使用 xterm CSI modifier 编码。终端未启用鼠标协议时，点击 `http://` 或 `https://` 链接可调用系统打开器；非 HTTP(S) 协议会被拒绝，启用终端鼠标协议的应用则优先接管鼠标事件。终端色彩允许直接 RGB；其他 widget 使用语义主题色。Ctrl+V 是否粘贴由配置决定。

终端 focus 时实现 `RawKeyConsumer` 语义：一般输入转成 VT bytes；force keys（至少 terminal.toggle）优先由应用处理。PTY 输出事件唤醒主循环。普通终端鼠标选择按 terminal cell 列处理，组合字符与基字符保持同一选区，复制内容再经 OSC52 通道同步；终端输出中的 OSC52 剪贴板内容会经过 64 KiB、Base64 和 UTF-8 校验后同步到 Host 内部剪贴板。

## 5. 渲染

顶栏显示 `mtide`、菜单、首个工作区项目名以及当前焦点、LSP、主题和插件摘要；无目录启动时项目名回退为 `workspace`，多根工作区使用首个根目录作为项目标识。

Cell 至少包含 glyph、foreground、background、attributes、width/continuation。current/previous 双缓冲比较后只写差异。所有 panel 使用统一 layout rect；draw 不得改变业务状态。resize 后先重排再重绘，避免使用旧坐标。

底部面板高度由 `bottomHeight` 控制，运行时按 3–24 行钳制；Output、Terminal、Changes 和 Agent 相关布局共享该高度，调整后下一帧重新计算编辑器 viewport 与 PTY 行数。底部面板顶部横向分隔线支持鼠标拖拽改变高度，释放鼠标后结束调整状态。
