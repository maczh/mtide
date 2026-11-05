# 当前实现状态

本文记录代码仓库当前真实状态，和产品需求/迁移计划中的目标状态分开维护。

- 终端面板已支持非鼠标跟踪模式下的拖拽文本选择、选区反色显示和 `Ctrl-Shift-C` 复制；单击终端 OSC 8 链接仍保持打开链接行为，启用应用鼠标协议时继续将事件原样转发给 PTY。
- 终端选择坐标已按当前布局同步：底部 Terminal 使用可见三行，全屏 Terminal 使用实际内容高度，复制和选区高亮不会因布局切换错位。

## 已落地

- 插件 panel proxy 新增 progress widget，支持 value/label/char 和 0 到 1 范围钳制，在 Sidebar、Bottom、Drawer 与插件标签中一致渲染。
- 底部面板布局已支持 `bottomHeight`（3–24 行）配置和非编辑器焦点下的 `Alt-Shift-Up/Down` 动态调整；Terminal、Output、Changes、Agent 布局共用该高度，编辑器 viewport 与终端 resize 会随重排同步更新。
- 侧栏布局已支持鼠标拖拽右侧分隔线调整宽度，宽度按 20–80 列及当前窗口可用中心区间钳制，释放鼠标后结束拖拽状态并保留当前会话配置。
- 底部面板顶部横向分隔线已支持鼠标拖拽调整高度，和 `Alt-Shift-Up/Down` 共用 3–24 行边界与 PTY/viewport 重排链路。
- 诊断已新增 Problems 侧栏视图：合并当前文件的 LSP 与插件诊断，按严重级别显示，支持键盘/鼠标选择并 Enter 跳转到行列；`Ctrl-K D` 打开该视图，保留原有行号标记和状态栏消息。
- Workspace 已补齐多根目录操作闭环：命令面板支持 Save Workspace As、Add Folder to Workspace、Remove Active Workspace Folder；保存使用相对 folder 路径并拒绝覆盖已有文件，移除只更新会话和插件文件 scope，不删除磁盘目录。

- 插件自定义编辑器 Tab 与 file handler 文件视图已支持 `on_event` 鼠标事件：Host 传递内容区相对坐标和按钮类型；回调返回 `true` 时消费事件，返回 `false` 时保留编辑器滚动、选择等宿主行为。

- 插件 RPC 生命周期已加固：响应、取消、断连和挂起请求回调均会回收一次性 Lua registry 资源；单个响应、通知或关闭回调异常会记录日志并继续清理同一连接的其他请求，重复关闭不会重复触发关闭回调。

- 插件 RPC framing 已兼容 TTT/LSP：出站使用 `Content-Length` 帧，入站支持标准头帧与旧换行 JSON，并限制头部 8 KiB、消息体 32 MiB。

- 插件 RPC endpoint scope 已接入：`network.rpc=true` 允许全目标，数组声明按 TCP/Unix endpoint 精确匹配，未授权目标在建立连接前拒绝。

- 插件 `radio` widget 已接入统一面板选择状态：支持数字或字符串选中值、方向键导航、`on_change` 回调和单选标记渲染。

- 插件 `image` widget 已接入；在当前 `graphics.protocol=none` 时以带 alt/src 的文本占位安全降级，避免上游图像插件因未知 widget 失败。

- 插件 `ttt.fs` 已补齐 `exists`/`list` 与 manifest 路径 scope：插件包目录始终可用，`true`/`{workspace}`/绝对路径/`~`/`{config}`/`*` 范围会在 canonical 边界校验后生效；目录结果稳定排序并返回 `{name, is_dir}` Lua 数组，普通 I/O 失败返回 `nil,error`。
- 插件自动发现已落地：显式 `--plugin` 优先，随后扫描全局 TTT 配置目录与工作区 `plugins/`，按 manifest 名称去重，坏包静默跳过，主题插件复用同一加载路径。
- 插件 `crypto` 已按 TTT 沙箱语义补齐：提供支持省略 padding/外围空白的 Base64 编解码、系统 CSPRNG 十六进制随机字节和 RFC 4122 v4 UUID，随机请求限制为 1–1024 字节并覆盖无效输入测试。
- Lua 插件沙箱已收紧：仅保留安全 `os.time/clock/date`，移除 `io`、`debug`、动态加载和危险全局函数，并将 `package` 搜索器限制为内置模块 preload。
- 插件 Tree 交互已补齐：Enter 在带 `children` 的节点上展开并派发 `on_expand(node)`（已展开节点不重复触发展开回调），同时派发 `on_select(node)`；叶节点继续派发 `on_select(node)`，覆盖键盘激活回归测试。
- 插件 Tree 官方交互语义已补齐：父节点 Enter 同时触发选择、仅在实际展开时触发 `on_expand`，Left/Right 可折叠/展开当前节点，覆盖导航回归测试。
- 插件 Tree/List `node_menu` 已接入：右键会同步点击行选择，打开宿主上下文菜单，支持 separator/checked 展示，菜单命令复用 `on_command(command, node)` 并支持键盘/鼠标激活。
- 插件 Tree/List 叶节点激活已对齐 TTT：Enter/点击在保留 `on_select` 的同时派发 `on_command("activate", node)`；Table 原有 `on_activate(row)` 保持不变。
- 插件 List 命令回调已补齐：`key_commands` 可驱动 `on_command(command, node)`，传递当前节点对象；Table 原有行号参数保持兼容。
- 插件普通面板 Dropdown 已接入选择状态机：方向键更新选项，Enter 派发 `on_change(value)` 或 `on_menu(command)`，并覆盖 Sidebar 集成测试。
- 插件 Tree 命令回调已补齐：`key_commands` 可驱动 `on_command(command, node)`，当前节点对象与 List 保持一致。
- 插件调试快照落盘已补齐：`ttt.screenshot(path)`/`ttt.debug(path)` 在 `screen.read` 与 `fs.write` scope 下写入受限路径并返回成功标记，越界路径拒绝。
- 插件调试权限已收紧：`ttt.debug` 现在同时要求 `screen.read`、`editor.read`、`terminal.read`，避免低权限插件读取编辑器状态。
- Lua 安全日期 API 已补齐 `%I`、`%p`、`%Z` 格式，`os.date` 与 TTT 约定的安全格式集合一致。
- 插件自动化已支持受限 `scroll` 动作：按屏幕坐标注入真实滚轮事件，正负方向明确且次数上限 1000。
- 插件文件 API 已支持二进制 Lua 字符串：`ttt.fs.read/write` 不再强制 UTF-8，非文本文件可在 scope 内读写并覆盖集成测试。

- 插件 `ttt.system` 已接入 `system.exec` 命令 allowlist：`exec`、`exec_async`、`spawn` 共用 manifest 声明的可执行文件范围，未授权命令不会启动。

- Find 面板已支持 Ctrl-Alt-R 切换 smart-case/正则搜索，并在面板中显示当前模式；Search、Include、Exclude 三字段可用 Tab/Shift-Tab 或鼠标点击切换，include/exclude 支持逗号分隔 glob，并实时接入后台 generation/cancellation 搜索链路。
- Git 的分支名、提交消息、PR 评论和 Explorer 新建/重命名输入统一支持 Unicode 光标编辑、左右移动、Home/End、Backspace/Delete 及 bracketed paste。
- 编辑区已接入鼠标滚轮 viewport：普通滚轮垂直滚动，Shift+滚轮水平滚动，并对边界进行钳制。
- 编辑器已在无 .editorconfig 时自动检测前 200 行的缩进风格与步长，并以确定性的最小步长规则处理频率平票。
- Settings overlay 已增加缩进自动检测和 tabs 手动覆盖选项，状态栏显示当前缩进模式。
- 缩进自动检测开关已持久化为 editor.autoDetectIndent，重启后继续尊重手动覆盖。
- Settings overlay 已支持在 2/4/8 之间循环手动缩进步长，并持久化 editor.indentSize。
- 插件命令行和自定义 Dialog 输入已接入 bracketed paste 路由，粘贴内容不会落入底层编辑器；两者均支持 Unicode 光标插入、左右移动、Home/End、Backspace/Delete，Dialog input 还支持多行 Enter 和 Enter 触发 `on_submit`。
- 插件 tree/list/table/tabs 的回调已接入插件标签 Enter、侧栏/底栏面板鼠标点击和面板键盘选择：tree/list 传节点表、table 传 1-based 行号，table Enter 优先调用 on_activate、tabs 传 tab id，并在执行后消费编辑请求；自定义 `open_tab`、`editor.file_handlers` 文件视图和 sidebar/bottom 注册面板均支持 `on_event` 按键回调与事件消费；所有 panel proxy 已支持 `panel:redraw()` 请求异步重绘，文件视图 `tab:focus()`/`tab:close()` 已接入 Host 生命周期请求；`ttt.events` 的 `key.press` 已接入编辑器输入路由，可由插件安全消费按键。
- 插件 status item 已支持 `opts.on_click`，状态栏按可见文本列范围命中鼠标并执行 Lua 回调。

- 插件编辑器右键上下文菜单已完成：受 editor.read 权限保护，支持 1-based 行列和当前单词参数、菜单项回调、分隔项、鼠标/键盘选择、overlay 焦点恢复和编辑请求消费。

- Ratatui/Crossterm 全屏布局：菜单栏、Explorer/Find/Changes 侧栏、编辑器、Terminal/Output 底栏、AI Agent 区、状态栏、编辑器自动垂直滚动、鼠标捕获、bracketed paste 粘贴事件路由/编辑区定位和窄窗口降级布局；Open File、插件命令面板和 LSP 弹层使用 overlay focus 栈，取消后恢复打开前焦点。
- CLI 自动化：`--size WxH` 会在启动时调整 PTY/Agent 尺寸并不会污染位置目标；`--exec` 已支持键盘、文本/粘贴、复制、真实鼠标 click/rclick/hover/drag、panel 焦点、文件 open/定位/只读、debug/screenshot 输出和基于文件存在的有界 wait-for；默认实例通过当前用户/目录对应的 loopback Unix socket 接收后续进程的文件目标和行列位置，`--new-window` 可绕过复用，`--listen` 显式声明监听实例。
- CLI 自动化在无 TTY 的 CI/脚本环境下支持 headless 执行：带 `--exec` 时跳过 raw mode 与备用屏幕，默认使用 80x24 或 `--size` 尺寸，仍执行文件、编辑、截图、debug 和 quit 动作。
- 键盘导航：Tab/Shift-Tab 焦点循环、面板切换、侧栏/底栏标签切换、退出快捷键。
- 文本编辑：UTF-8 字符列、插入/换行/删除、上下左右/Home/End、`Ctrl-Alt-M` 嵌套跨行匹配括号跳转、`PageUp/PageDown` 按视口分页移动及 Shift 选区、`Ctrl-Home/End` 文件首尾跳转及 Shift 扩展、`Ctrl-Shift-Left/Right` 按词移动与选区扩展、未按 Shift 时按词移动、`Ctrl-G` 行列定位、`Alt-Backspace`/`Ctrl-Backspace` 与 `Alt-Delete`/`Ctrl-Delete` 按词删除（含跨行边界和单次撤销）、保存规范化、Save As 新路径保存与覆盖保护、自动缩进、选区 Tab/Shift-Tab 批量缩进和反缩进、LSP/外部 formatter format-on-save、语言覆盖、`saveAllOnQuit` 批量保存、撤销/重做、dirty 状态、括号/引号自动闭合和闭合符跳过、基于缩进的代码折叠/全部展开、当前文档 Replace All 单批次撤销、重叠多选区合并、复制行/删除行/按已知语言切换注释（未知语言会明确提示不可用），以及 Shift 选区、支持按光标/选区逐项 spread 的多光标剪贴板、内部剪贴板和受 64 KiB 限制的 OSC52 外部剪贴板；`Alt+Shift+Up/Down` 在无选区时可重复添加上方/下方光标，有选区时继续移动选中行；编辑器支持多标签，每个标签独立保留缓冲区、光标、撤销栈和 dirty 状态。搜索 Replace 模式下按 `Ctrl-Shift-Enter` 可对当前搜索结果覆盖的工作区文件执行全部替换，并遵循与搜索一致的 smart-case 规则；当前打开文件保留内存 dirty/undo 语义，其他文件直接写回磁盘；脏标签检测到磁盘变更后可用 `Ctrl-Alt-R` 显式重载。
- 工作区：目录浏览、文件打开、Explorer 新建文件/目录、重命名、删除确认和刷新、递归 smart-case 搜索、Unicode 字符列结果、可配置 include/exclude glob、Find 面板运行时编辑三字段过滤、编辑器 `Ctrl-F` 继承单行选区、活动缓冲区未保存编辑后的实时搜索结果、跨根结果循环导航和搜索结果跳转、当前搜索结果跨文件 Replace All、`.ttt` 多根 folder 聚合搜索/Explorer、打开文件磁盘 mtime 轮询与干净标签自动重载、脏标签外部变更保护、Git 分支/status、Branches 侧栏创建/切换/删除分支、unstaged/staged unified diff 读取、保存后状态刷新，以及 Changes 面板整文件/单 hunk/批量 stage/unstage（Space/s/u）、提交消息提交、当前文件 blame、提交历史和后台 fetch/pull/push/sync（pull 后 push）操作；远程结果在主循环回收，认证失败进入内存凭据重试 overlay；Changes 面板按 `d` 可进入可滚动 Output diff 视图，长上下文默认折叠并支持 `Enter`/`e` 局部展开、`c` 全文/变更上下文切换和 `v` unified/split 切换。
- 终端：portable-pty 多 tab 持久 shell、settings shell/args/scrollback/ctrlV 配置、后台读取、命令提交、方向键、application cursor mode、PageUp/PageDown 与鼠标滚轮 scrollback 浏览、Tab、Ctrl-C/Ctrl-D/Ctrl-L、可选 Ctrl-V 剪贴板写入、resize，以及 VT screen（光标、滚屏、独立 scrollback、宽字符双列占位、组合字符附着、alternate screen、CSI 移动/清屏、SGR 基础颜色/粗体/下划线/反色），并在底栏按 ANSI 样式渲染；Terminal 焦点下支持 `Ctrl-Shift-T` 创建和 `Ctrl-Alt-Left/Right` 切换；解析终端的 1000/1002/1003/1006 鼠标模式，并按 SGR 或 legacy X10 编码转发鼠标事件至 PTY；Alt 字符、Shift-Tab 及带修饰键的光标/分页/功能键使用 xterm CSI modifier 编码。
- VT `CSI ! p` soft reset 已补齐：保留屏幕内容，同时清理属性、鼠标跟踪/编码、焦点上报、滚动区域、保存光标和自定义 Tab stop，避免全屏程序复位后把宿主终端模式带入下一会话。
- VT DECRQM 批量查询已补齐：`CSI ?mode;mode...$p` 会按请求顺序逐项返回 `DECRPM` 状态，不再只响应第一个模式。
- LSP：配置模型、按语言选择 server、后台 stdio JSON-RPC reader、request/response 匹配、initialize、initialized、didOpen、按 URI 递增版本的 full didChange、统一 manager 关闭时的 shutdown/exit 与子进程回收、事件循环轮询，以及 publishDiagnostics 状态解析和编辑器行号标记、UTF-16 code unit 列与编辑器字符列双向转换、completion/hover/signatureHelp/definition、workspace edit Rename、formatting 和首个 Code Action。
- LSP 服务失败后可从命令面板执行 `Restart Language Server`：Host 会关闭旧进程、清除旧文档版本、重新 initialize，并用 `didOpen` 重新同步当前文档，避免新进程收到无效的 `didChange`。
- LSP 服务端反向请求已接入安全回包：`workspace/configuration`、`workspace/workspaceFolders`、`client/registerCapability`/`unregisterCapability`、`window/workDoneProgress/create` 和 `window/showMessageRequest` 不再被当作无响应通知；多根工作区信息按当前 Host 状态返回。
- TTT 插件基础运行时：manifest 校验、Lua 5.4、`ttt`/`ttt.json`/`ttt.system`/`ttt.fs`/`ttt.text`/`ttt.storage`/`ttt.regex`/`ttt.settings` 模块、权限拒绝、包目录路径约束，以及 commands（`handler`/旧 `run`）/keybindings/sidebar/bottom/file_handlers 注册、兼容 panel proxy、list/table 的 selected/loading/empty 状态、同步/回调式 `system.exec` 与 `exec_async`；`ttt.storage` 按插件私有 JSON 文件提供 `get`/`set`/`remove`/`keys`，支持嵌套 JSON 值并限制单插件 1 MiB，未声明 `storage` 权限时不暴露函数；`ttt.regex` 提供带捕获组的 `compile`/`find`/`find_all` 和忽略大小写选项；`ttt.settings` 支持受 `settings_keys` 精确/通配约束的点号路径读写、与 Host 相同的默认值并立即持久化；扩展名匹配的 file handler 可执行 `open(path, tab)` 并把 `tab.render`/字符串结果显示为插件文件视图，`tab.on_close` 生命周期回调可在视图关闭时执行并清理资源；`tab.open`、`tab.activate`、`tab.close` 覆盖普通编辑 tab 与插件文件视图的生命周期。
- 配置：`MTIDE_CONFIG_DIR`、`TTT_CONFIG_DIR`、显式 `--config FILE` 和项目 `.mtide/settings.json` 的基础布局配置；插件 `ttt.settings` 与 Host 共用显式配置文件；支持 `defaultFolders`/`default_folders`/`startupFolders` 无显式目标时启动默认工作区；启动时读取工作区 `.editorconfig` 的常用缩进/保存规则；Agent 兼容原版 `agents` 顶层 schema，支持 `cli`、字符串/数组 `args`、`install`，并归一化为内部 provider 模型；`autoClosingBrackets`、`selectionStyle`、`reuseWindow` 等兼容设置已接入，未知顶层和 editor 字段会在保存/序列化时保留。
- 插件异步进程与文件安全：`system.exec_async` 返回带 `cancel()`/`id()` 的句柄，`system.cancel_async(id_or_handle)` 兼容显式取消入口，取消会终止后台子进程并在回调结果中标记 `cancelled`；`ttt.fs` 的 read/write/list/exists 已按 manifest scope 做 canonical path 校验，支持工作区、配置目录、用户目录、绝对目录和全局 scope。
- 插件命令输出治理：`system.exec`/`exec_async` 支持 stdout/stderr 共享的 `max_output_bytes` 上限，超限时终止进程并返回截断输出、`truncated=true`、`exit_code=-1` 和 `ok=false`。
- 插件流式执行：`exec_async` 支持 `on_stdout(chunk)`/`on_stderr(chunk)`，输出块在线程读取、主线程 Lua 回调，取消、超时和截断后停止继续分发，并保留一次性最终结果回调。
- 插件标准进程入口：`system.spawn(binary, options)` 已兼容标准二参数签名，支持 `args`/`cwd`/`env`/标准流回调，并保留旧 options-only 写法。
- 网络权限边界：`network.http` 的空数组、显式 host allowlist 和 `true` 全放行三种状态已区分，同步、异步及重定向请求统一执行目标校验。
- 插件生命周期：已支持注册与显式执行 `ttt.on_uninstall` 清理钩子，并与已有一次性 `ttt.on_install` hook 一样在执行后清理 Lua registry 资源。
- 设置 UI：命令面板中的 `Open Settings` 可打开键盘导航的设置覆盖层，支持即时切换侧栏、底部面板、Agent、软换行、保存格式化、退出批量保存、自动保存及 1/5/10/30 秒延迟、终端 Ctrl-V、LSP、缩进自动检测、tabs 覆盖和 2/4/8 缩进步长；可用 `Ctrl-S` 将当前设置持久化到 settings 文件，状态栏同步显示当前缩进模式。
- 插件 `editor.change`/`cursor.change` 事件在保持旧参数顺序的基础上追加选区起止 1-based 行列与 `current_word`；无选区坐标为 `0`，Unicode 单词边界由 Host 统一计算，便于 TTT 插件直接实现上下文提示和重构入口。
- `ttt.editor.current_word()` 已复用 Host 的 Unicode 单词边界规则，插件可直接读取光标所在单词而无需自行转换 rune 列。
- CLI：支持 `--help`、`--version/-v`、文件/目录位置参数、`file:line:column` 定位、`.ttt` 多根工作区解析、`--config FILE`、可重复的 `--plugin DIR`、`--listen`、`--new-window`、`--welcome` 和标准输入目标；无目标时自动检测 pipe/regular file，显式 `-` 可强制读取，标准输入均在进入 raw mode 前读入并作为未保存 tab 打开；显式文件不可读或 `.ttt` 工作区解析失败时回退欢迎缓冲区/当前目录并在状态栏保留错误，编辑器支持 `Ctrl-O` Open File 路径弹层。

## 当前兼容边界

以下能力已经在需求文档中定义，但尚未达到完整产品级实现：

- 轻量高亮已增加 Markdown 标题/列表/fenced code、JSON/TOML 对象键与 Rust/C 风格单行块注释的上下文颜色，并在跨行块注释、字符串和 Markdown fenced code 间保持状态；搜索、诊断、选区和链接高亮仍在后续阶段叠加。

- 编辑器多标签、标签切换和未保存关闭保护已接入；每个 tab 可用 `Ctrl-Alt-L` 单独循环语言覆盖，覆盖会保存到工作区 `.mtide/language-overrides.json` 并在重启/重新打开时恢复，立即驱动状态栏、注释、语法着色和 LSP server 选择；已支持关闭 soft-wrap 时的水平视口和光标可见性、按终端列宽计算的 `editor.wordWrap`/`editor.wordWrapMaxColumn` 软换行，以及软换行开启时按屏幕行移动 Up/Down（含宽字符列位置保持）；还支持常见语言轻量语法着色、Markdown `<details>` 区块折叠（忽略行内代码和 fenced code 中的字面量标签）、鼠标点击/拖动选择、`Alt+Shift` 矩形/列选区、`Ctrl-A` 全选、`Ctrl-Alt-N` 选择下一个相同文本、`Alt-Shift-I` 跨行选区拆分为逐行 selections、缩进代码折叠、无选区时 `Alt-Shift-Up/Down` 重复添加上下方光标、有选区时移动选中行、`Ctrl-J` 原子化合并行、`Alt-Shift-S` 排序行和 `Alt-Shift-U/L` 大小写转换，以及 `Alt-Up/Down`/Alt-click 附加光标和多光标字符/换行/退格/删除；编辑器正文中的 HTTP(S) URL 可在 Ctrl-悬停时高亮并在 Ctrl-点击时安全打开，处理 Unicode 列与常见标点边界；列选区已接入复制、剪切、替换和单次撤销，尚无完整格式化和增量语法高亮引擎。
- 编辑区鼠标滚轮支持普通垂直滚动，并兼容 Shift/Alt+滚轮横向滚动；两种修饰键均按终端事件统一处理并对水平视口做边界钳制。
- Replace 模式保持非模态：编辑区获得焦点后输入和粘贴继续作用于文档，编辑区按 `Ctrl+R` 可重新聚焦替换栏，`Esc` 可关闭 Replace 模式。
- Undo 保存点按历史状态而非文本内容识别；撤销后创建新编辑分支会使被丢弃的旧保存点失效，事务内部保存点在后续编辑后也不会错误清除 dirty 状态。
- 编辑器正文会根据当前搜索状态实时高亮 smart-case、大小写敏感和正则命中；编辑、撤销/重做及插件 `editor.set_search` 共用同一查询状态，清空或非法正则不会残留高亮。
- 插件 `ttt.system.spawn` 保证 stdout/stderr 流关闭后才派发 `on_exit`，即使子进程快速退出也不会让退出回调抢在输出尾部之前执行；所有回调注册仍在流关闭后统一回收。
- 插件 `panel:box({title, render})` 已接入容器兼容层，子 panel render 在主面板与文件视图中均可正常绘制。
- 插件多行 Input 已兼容 `multiline`/`wrap`/`min_rows`/`max_rows`：Sidebar、Bottom、Drawer 和自定义 Tab 可保留换行文本，按面板 Unicode 列宽显示，Enter 在多行模式插入换行并触发 `on_change`，不会误触发 `on_submit`。
- 插件 Input/Password 文本编辑已支持 Unicode 光标插入、Left/Right、Home/End、Backspace、Delete 和多行 Enter；Sidebar、Drawer、Dialog 与密码控件共享该光标语义，受控 Input 重新渲染时会安全钳制光标位置。
- 插件 Markdown 已支持配置表 `wrap_width`，按 Unicode display width 进行稳定的单词换行。
- 终端已具备 PTY 字节通道、跨读取边界的 UTF-8 解码、ANSI 样式（基础色、亮色、256 色、真彩色、粗体、faint、italic、strikethrough、conceal、blink，RGB 参数安全截断到 0–255）、光标可见性控制（`CSI ?25 h/l` 以反色渲染当前光标单元）、DECSCUSR `CSI Ps SP q` 光标样式状态（块/下划线/竖线）及 alternate-screen/soft-reset 快照语义、DEC bracketed paste 状态跟踪及标准粘贴包装、DEC focus reporting `CSI ?1004 h/l` 与宿主 FocusGained/FocusLost 的 `CSI I`/`CSI O` 通知、alternate screen 下字符/退格/回车/Tab 和 ASCII 控制组合直通 PTY、延迟自动换行（wrap-pending）与禁用自动换行时的宽字符边缘裁剪、宽字符 continuation 元数据及按字符退格/覆盖/清除、`Alt-T` 可逆全屏终端布局、alternate screen（含主屏 links/样式/光标/滚动区域快照恢复）、独立 scrollback、基础鼠标协议转发（SGR/X10/UTF-8 1005）、OSC 8 超链接点击打开（包括 scrollback 浏览中的可视行）、组合字符附着、保存/恢复光标、`CSI r` 局部滚动区域、`CSI L/M/S/T` 区域行编辑与滚动、IND/RI/NEL、CSI Tab/BackTab、DSR `CSI 5 n`/`CSI 6 n` 及私有 `CSI ?6 n` 光标报告、设备属性 `CSI c` 响应、DECRQM `CSI ?Ps $p` 已跟踪模式查询、窗口/屏幕尺寸 `CSI 18 t`/`CSI 19 t` 响应，以及 `CSI J/K/@/P/X/b` 清除、插入、删除和重复字符与元数据同步清理；链接仅允许 `http://`、`https://`，宽字符 continuation 单元清除时会同步清理样式、组合字符和链接元数据；组合字符在退格/光标回退后的附着语义已覆盖，完整终端交互状态仍待实现。
- 终端 OSC 7 工作目录已解析并按会话保留（支持 BEL/ST 两种终止符，拒绝控制字符）；本地 file:// 或 localhost 路径会作为新 Terminal 标签的启动 cwd，远程 authority 会安全降级为仅显示、不用于本地进程启动。
- LSP manager、后台通知轮询、didChange/didSave/didClose 同步、5 秒请求超时与 `$/cancelRequest`、diagnostics 行号标记与当前行消息、completion 候选弹层、hover 信息弹层、signature help、document symbols/大纲导航、definition/implementation/type definition/references 位置跳转与多位置选择弹层、workspace-edit Rename、整文件/选区 range formatting 和 Code Action 选择弹层已接入；Code Action 同时支持带 `edit` 的工作区编辑和标准 `command`/`arguments` 命令动作，并通过 `workspace/executeCommand` 执行；Workspace Edit 的 `changes`/`documentChanges` 可写回其他文件，并支持 `CreateFile`/`RenameFile`/`DeleteFile` 资源操作；应用前会预检 URI、目标文件、只读状态、范围合法性、重叠编辑和资源操作覆盖策略，单文件多编辑合并为一次撤销；Rename 现在先展示按文件汇总的可视化预览，资源目标冲突会在预览中显式标注，Enter 应用、Esc 取消；LSP 服务状态已区分启动中、已连接和失败，服务进程异常退出或请求失败会标记失败并从活动客户端移除，后续同步可重新启动服务，状态栏会显示对应状态。
- Git 目前读取 branch/status/unified diff、保留显式/省略数量的 hunk header、commit 历史和 blame，并支持使用 `porcelain=v1 -z` 安全解析带空格、换行、重命名和复制的路径；支持 Branches 侧栏创建/切换/删除、整文件/单 hunk/批量 stage/unstage、Changes 面板提交及 fetch/pull/push/sync；Changes 面板可用 `d` 打开可滚动的 Output diff 视图，`v` 在 unified/split 视图间切换，`t` 打开独立 Git diff tab，标签内支持 `v` 切换 unified/split、`[`/`]` 选择 hunk，Enter 进入对应文件的可编辑 `HEAD` 基线 diff 模式；Output split 视图也可用 Enter 进入当前变更文件编辑，编辑后行号区实时标记新增/变更行；冲突文件会高亮并支持 Changes 面板 Ours/Theirs 快速检出后暂存，编辑器行号区以 `O`/`B`/`T`/`!` 标识冲突区域，编辑器内还可按标准 diff3 冲突块使用 `Ctrl-Alt-Up/Down` 循环定位、`Ctrl-Alt-O`/`Ctrl-Alt-E`/`Ctrl-Alt-B`/`Ctrl-Alt-K` 选择 Ours/Theirs/Base/Keep Both，`Ctrl-Alt-P` 打开当前冲突块的 Ours/Base/Theirs 三方预览并可直接选择，结果纳入单次 undo；普通保存、Save As 和退出批量保存会检测未解决冲突标记并阻止覆盖文件；远程操作认证失败后提供内存凭据输入、重试和取消，username/password 支持光标编辑与粘贴，凭据仅通过进程环境传给一次性 Git credential helper，不写入日志、配置或工作区。
- Lua 插件目前能加载基础入口和安全系统 API，插件 keybinding 和 Ctrl-P 命令面板可实际触发已注册 command，`ttt.on` 可订阅 app/document/tick 以及 `tab.open`/`tab.activate`/`tab.close` 生命周期事件，`ttt.set_echo`/`ttt.clear_echo` 可暂时独占并恢复状态栏，sidebar/bottom panel 的 `render` 回调可通过 panel proxy 绘制常用文本/列表/表格，list/table 支持 selected/loading/empty、表格 cells 和右侧标记，以及 pager/tabs/scrollview/dialog/dropdown/password/checkbox/border 等常见 widget 的兼容降级内容；`require("ttt.editor")` 已按 `editor.read` 权限提供活动缓冲区全文、行数组、当前行、当前单词、指定行、行数、光标、选区、选区文本、路径、文件名、语言和多光标快照，`editor.diff()` 可读取活动可编辑 Diff 的变更块并按 `editor.write` 权限通过 `go_to_diff_hunk(index)` 跳转，按 `editor.write` 权限提供插入、整行替换、范围替换、光标/选区和多光标操作，写入复用 Buffer undo checkpoint 并在插件回调后消费，Lua 坐标为 1-based rune 列；插件日志可进入 Output 面板，获得 `terminal.control`/`terminal.read` 权限的插件可通过 `ttt.terminal` 使用受管控的 portable-pty spawn/send/read/screen/resize/focus/close/list 终端 API，`screen` 返回 VT 解析后的可视文本，输出尾部限制为 1 MiB，file handler 已可实际打开为插件视图，支持扩展名和最多 512 字节 `magic` 文件头匹配且 magic 优先，tab proxy 提供 `id`/`title`/`focus`/`set_title`/`close`/`on_close` 入口且 `set_title` 会实际更新视图标题，`ttt.text.width/truncate` 已按终端列宽处理宽字符，`system.exec` 支持受权限保护的 stdin/env/timeout 选项，`exec_async` 已改为后台执行并在主循环回调；manifest 已支持 theme-only 插件、主题 descriptor 和包内路径校验，主题可在启动时加载，并可用 `Ctrl-Alt-T` 在内置/配置/插件主题间循环切换，应用到菜单、编辑器、侧栏、弹窗、Agent 面板、背景、面板、边框和状态栏；终端 ANSI/256 色仍按终端协议语义渲染。尚未覆盖 TTT 的完整交互 widget 状态和终端模块。
- AI Agent 已具备独立 PTY/VT 会话、独立输入路由、alternate screen 下字符/退格/回车/Tab/鼠标事件直通 PTY、bracketed paste 包装、兼容旧格式的 `agent.command`/`agent.cwd` 配置，以及 `agent.providers`/`agent.active` 多 provider 切换；新增 PATH 可用性检查、`install` 命令配置、显式确认后在普通 Terminal 中执行安装，以及安装完成后的后台可执行文件检测和 Agent PTY 自动重启；新增全局/provider 级 `persist` 配置，存在 tmux 时使用稳定命名的 tmux session 跨 mtide 重启 attach 真实 Agent 进程，未满足条件时回退普通 portable-pty；退出时会以版本化 JSON 原子持久化 active provider、输入缓冲和屏幕文本快照，并对恢复文件执行大小、版本和内容校验，在下次仍配置该 provider 时优先恢复 provider 与会话内容；Agent 及 provider 已支持显式 `tools` 权限清单，并默认拒绝未声明工具；新增受权限和 workspace 根目录限制的结构化 `read_file`/`list_files`/`write_file`/`search` 工具调用入口，写入使用临时文件替换且支持 workspace 内创建新文件，搜索不跟随符号链接并限制结果数量；Agent PTY 的 JSONL `tool_call` 消息现在由主循环异步执行并回写 `tool_result`，沿用同一权限和 workspace 校验。

补充：dialog checkbox 已具备状态保存、含 dropdown/button 混合布局的 Tab/Shift-Tab 焦点切换、Enter/Space 激活、内容行鼠标命中和 `on_change` 回调；dialog password 已接入受控输入、掩码渲染和回调；终端已解析 OSC 0/1/2 窗口/图标标题并显示在 Terminal 面板标题中，支持受 64 KiB 限制的 OSC52 剪贴板接收，并兼容 `ESC Z`、CSI 13/14/16/18/19 t 设备和窗口尺寸查询、DECPAM/DECPNM 应用键盘模式；PTY 退出通知具备幂等语义，只报告一次，主动 close 也具备幂等语义。

## 验证基线

搜索 Replace One 已通过 `Ctrl-Enter` 接入；执行前重新扫描活动 Buffer，避免编辑后使用旧匹配范围。

快捷键兼容：`keyboard.modifierKey` 支持 `control`、`control+super` 和 `super`，Super/Cmd 在启用对应模式时仅由 Host 快捷键匹配层归一化为现有 Control 快捷键；终端、Agent 与编辑器插件 `key.press` 保留原始修饰键。

最近关闭标签可用 `Ctrl-Shift-T` 或命令面板 `editor.reopenClosedTab` 恢复；最多保留 20 个已保存标签，并重新同步 LSP，未保存标签不会进入恢复栈。

终端与 Agent 修饰键已补齐：Alt+字符使用 ESC 前缀，Shift-Tab 使用 BackTab，带 Shift/Alt/Ctrl/Super 的方向键、分页、Home/End、Insert/Delete 和 F1–F12 使用 xterm CSI modifier 编码；`Alt-T` 仍由 Host 作为终端全屏 force key 处理。

Markdown 图片粘贴已接入：`Ctrl-V` 在后台读取 `wl-paste`/`xclip` 的 PNG 剪贴板，按活动文档路径校验结果后使用受控的 `editor.markdownImageDir` 写入资源并插入相对链接；普通文本剪贴板保持原有回退路径，不阻塞 UI。

`.editorconfig` 读取已支持父级到子级覆盖合并，并尊重 `root=true` 截断父级搜索；常用缩进、Tab 宽度、末尾换行和尾随空白规则会覆盖默认编辑器设置。

主窗口顶栏已显示首个工作区项目名；无目录启动安全回退为 `workspace`，并与焦点、LSP、主题和插件摘要同栏呈现。

状态栏已显示当前文件的工作区相对路径；无路径欢迎缓冲区回退显示 `welcome.md`。

自动保存已接入：`editor.autoSave`/`editor.autoSaveDelayMs` 默认每 5 秒保存可安全写入的 dirty Buffer，冲突和外部变更继续跳过并保留保护状态；保存后同步 LSP/Git。

Settings overlay 已补充 Auto Save 开关与 1/5/10/30 秒延迟循环，既有设置行索引保持兼容。

插件 `panel:image` 已兼容原版的 `data` 二进制图片输入；无图形协议时安全降级为带 alt 和字节长度的文本，不会丢弃插件内容。

插件兼容已覆盖 `ttt.editor` 读写与 undo group、`ttt.events` 多监听器和事件 payload（编辑器变更事件包含语言及选区状态/文本，书签事件包含变更详情与行号点击位置）、`ttt.diagnostics`、`ttt.bookmarks` 及对应权限；插件诊断与 LSP 诊断合并显示，书签图标显示在编辑器行号区。

插件 manifest 权限已兼容布尔值、字符串数组和路径数组；`ttt.system.env` 与 Host 主循环定时器 API 已接入并覆盖回调、清理和权限测试。

`ttt.automation.run` 已接入本地串行请求队列，支持 session/buffer、文本输入、按键、鼠标 click/rclick/hover/drag/scroll、打开文件、截图/调试快照、非阻塞 wait-for 和完成回调；wait-for 支持 5 秒默认、30 秒上限、超时结果及跨 tick 取消，并已具备 RPC 连接入口。

`ttt.rpc` 已接入 Unix/TCP JSON-RPC 长连接、notify、异步 request/response、服务端通知、反向 request、close 事件、请求取消和 `network.rpc` 权限校验；读取线程只投递事件，Lua 回调统一在主循环执行。`ttt.decorations` 已接入 `editor.decorations` 权限、范围解析、set/clear 和诊断/行号标记渲染链，可满足 ttt-mcp 当前插件的模块依赖；`ttt.editor` 已补充 Unicode rune 列与 UTF-8 字节偏移转换、基础 viewport 查询、主线程滚动请求 `scroll_to/scroll_by` 和搜索请求 `set_search/clear_search`，普通 smart-case 与正则搜索均已接入；`ttt.key/type/quit` 已接入自动化队列和权限校验，自动化 `click/drag` 已通过主循环鼠标事件分发；`ttt.screenshot/debug` 已接入 `screen.read` 权限和编辑器快照结果。

插件 status items 已支持 Lua 静态注册、Host 聚合和底部状态栏显示；动态 `ttt.set_status_item`/`ttt.remove_status_item` 也已按原版 TTT 参数和 `commands` 权限语义接入，并覆盖更新、删除、side、priority 测试。`ttt.exec_command`、`ttt.list_commands`、`ttt.show_panel`、`ttt.focus_panel`、`ttt.open_drawer`、`ttt.close_drawer`、`ttt.open_tab`、`ttt.open_readonly`、`ttt.open_diff`、`ttt.focus_tab`、`ttt.close_tab`、`ttt.open_file`、`ttt.clipboard_write`、`ttt.clipboard_read` 已接入并覆盖插件内命令编排、面板请求、Drawer、动态标签、只读标签、行级上下文/删除/新增差异、文件打开和剪贴板读写测试；只读 Buffer 的编辑/撤销/重做/保存保护已覆盖单元测试；`commands`、`keybindings`、`panel.sidebar`、`panel.bottom`、`panel.drawer`、`panel.editor`、`editor.file_handlers`、`clipboard.read` 能力已在 `ttt.register` 或 API 调用阶段执行权限拒绝。

插件 Widget 兼容层当前已覆盖 tabs/scrollview 的 selected/loading/empty 状态渲染，以及 pager 的 page/pages/total 分页显示。
`Ctrl-P` 命令面板已从仅插件命令扩展为内置编辑器、视图、工作区命令与插件命令的统一过滤和执行入口。
插件文件视图的 `tab.render` 也复用上述集合与分页控件语义。
Sidebar/Bottom 插件面板已按注册位置分别渲染。

每次提交前运行：

```text
cargo fmt --all
cargo test --offline
cargo clippy --offline --all-targets --all-features -- -D warnings
cargo build --offline
```

当前基线：`cargo test --offline -- --test-threads=1` 为 497 passed、1 ignored；Clippy 无警告，离线构建通过，`git diff --check` 通过。

- VCS 后端已建立 `VcsBackend` trait 与 `GitBackend` adapter，工作区状态读取通过该边界访问 Git；后续可在不改动 UI 面板的前提下接入 `jj` 等实现。
- 鼠标点击、拖拽/矩形选区和 Alt 多光标现在统一派发插件 `cursor.change`，并覆盖路径、光标、选区和当前单词 payload；插件不会因编辑器输入来源不同而漏收状态变化。
- Shift 选区、Alt-Shift 行操作、Ctrl-X/Ctrl-V 及多光标输入等编辑器提前返回路径也已补发 `cursor.change`/`editor.change`，保持插件事件和键盘编辑状态一致。
- 系统粘贴与插件自动化 `type/paste` 统一经过 `handle_paste` 时也会派发 `editor.change`，插件可用同一事件链追踪所有文本输入来源。
- 终端已解析 OSC 133（A/B/C/D）shell prompt mark；非 alternate-screen resize 前会清理标记范围内的旧 prompt，并在 RIS/alternate-screen 恢复时维护该状态，兼容 fish 等会在 SIGWINCH 后重绘提示符的 shell。

## 下一阶段顺序

1. 完善 `ttt.editor` 与事件系统的更多 payload 和插件自动化桥接。
2. 完善终端重排、组合字符状态和剩余 VT 交互状态覆盖。
3. 完善 Git 三方冲突编辑器和插件 diff 的更多编辑上下文。
4. 完善 LSP rename 冲突标注、更多 workspace edit 交互。
5. 完善 TTT widget 状态、插件 tab 生命周期和终端模块（当前已补齐自定义 tab `on_event` 按键回调基础桥接）。
# 最近完成

- 动态新增的工作区根目录会同步注入已加载插件的 `fs.read`/`fs.write`/截图调试 scope；只有声明 `{workspace}` 或全局权限的插件获得新根目录访问权，其他插件权限边界不变。
- 插件首次加载和运行中从注册表安装时会初始化全部多根 workspace scope，不再只授权启动时的首个根目录。
- Agent 结构化文件工具支持多根 workspace：相对路径安全匹配任一工作区根，`search` 使用 `.` 时聚合全部根目录结果，同时保持 canonical 边界与符号链接防护。
- Agent JSONL `tool_call` 现在由受限后台线程执行，主循环只收发 `tool_result`；并发上限为 4，文件边界、权限和错误响应保持一致，避免递归搜索阻塞 TUI。
- Agent 会话重启会递增工具结果代际并清理旧协议缓冲；后台任务完成后若属于旧代际会被丢弃，避免 provider 切换或安装重启后污染新 PTY。
- 插件运行时销毁会取消未完成的 `system.exec_async`，终止仍登记的 `system.spawn` 子进程；插件终端 PTY 继续由终端管理器统一回收，避免插件卸载后遗留后台进程。
- LSP 多根回归：新增协议级测试验证同语言同根复用客户端、跨根自动重绑定并清理旧会话。
- LSP 文件 URI：统一对路径进行 RFC 3986 风格 percent-encoding/decoding，空格、Unicode、百分号路径可稳定用于诊断、同步、补全、Rename 和 Workspace Edit，并拒绝远程 authority 与非法转义。
- 终端 VT 私有模式批量设置：`CSI ?mode;mode... h/l` 现在逐项应用光标可见性、焦点报告、bracketed paste、鼠标模式、自动换行和 alternate screen 等状态，兼容全屏程序合并发送的模式序列。
- LSP 签名帮助交互：编辑器在已启用且已配置语言服务器时，输入 `(` 或 `,` 会自动请求 `textDocument/signatureHelp`；手动快捷键仍保持兼容，无 LSP 配置时不会打扰普通输入。
- LSP 自动补全调度：普通字符、退格和删除会以 150ms debounce 在主循环触发补全请求，连续输入会取消旧计时；编辑时会关闭过期候选，避免旧结果覆盖当前文档。
- LSP 自动补全生命周期：打开、切换或关闭编辑标签时会清理旧标签的待执行 debounce，避免补全请求串到新活动文档。
- AI 补全回退已接入：`ai.enabled`、`autoSuggest`、OpenAI-compatible endpoint/model/API key 环境变量、上下文行数和超时均可配置；无匹配 LSP 时自动或 `Ctrl-Space` 手动发起后台请求，结果复用 completion 弹层/插入/撤销链路，并通过 generation、路径和光标校验丢弃过期响应。
- LSP 编辑器请求已改为非阻塞轮询：completion、hover、signature help、symbols、definition、references、implementation、type definition、Rename、Formatting、Range Formatting、Code Action 和 `workspace/executeCommand` 由主循环发送并回收响应；请求超时会发送 `$/cancelRequest`，切换标签会取消并丢弃旧请求，避免服务端慢响应卡住输入。
- LSP completion item resolve 也已异步化：选择带 `data` 的候选后由 pending request 回收解析结果，再应用合并后的编辑与 commit character；切换标签或超时会安全丢弃候选，不会把旧补全写入新文档。
- LSP 诊断正文反馈：诊断覆盖的编辑器行会在正文 span 上启用终端下划线，同时保留严重级别行号标记、主题语法颜色和状态栏消息。
- 工作区交互式搜索：Find 输入使用 75ms debounce 的后台扫描，generation 校验会丢弃过期结果，并在目录递归/文件遍历内部提前终止旧任务；主循环合并已打开标签的未保存内容，递归扫描不会阻塞 TUI。
- Git blame 状态栏：加载当前文件 blame 后，状态栏会跟随光标显示当前行的短 commit、作者和提交行摘要，Output 面板仍提供完整逐行 blame 浏览。
- Git blame 读取已后台化：使用 workspace root、相对路径和 generation 校验回收结果；切换文件、标签或改看 history 时会使旧任务失效，避免慢速 Git 操作阻塞 TUI 或污染当前文件。
- Git history 读取已后台化：按仓库 root 和 generation 回收提交列表；切换文件或标签会清理旧历史任务和结果，慢速仓库不会阻塞输入线程。
- Git History 采用每页 10 条的有界后台加载；Output 面板显示“加载更多”提示，按 `]` 追加下一页，并沿用 generation 校验避免旧仓库结果污染当前视图。
- 终端渲染已读取主题 `colors.terminal` 的 16 色 ANSI 调色板；Basic、Bright 和低位 Indexed 色支持主题覆盖，256 色与 RGB 真彩色保持协议值。
- Git workspace 状态刷新已后台化：status、branch、branches、Changes 聚合和当前 diff 在同一 generation 任务中读取；主循环按活动仓库 root 回收结果，重复刷新或切换仓库会丢弃旧快照，避免大仓库 Git 命令阻塞输入。
- Git workspace 状态结果若在活动 root 切换后到达，会自动丢弃并触发新 root 刷新，避免 Git 状态面板永久停留在 loading 状态。
- GitHub PR review fetch/diff 已后台化：启动或打开 PR URL 后先显示 fetching 状态，结果按 workspace root、PR target 和 generation 校验后再切换 review Output；旧任务不会覆盖新的工作区或 PR 目标。
- GitHub PR review comments 已后台化：评论列表读取在独立线程执行，按 repository/number/generation 校验回收结果，网络延迟不会冻结 Output 面板，切换 review 后的旧评论会被丢弃。
- GitHub PR 行级评论提交已后台化：本地 ref 解析和 GitHub POST 在独立任务执行，提交期间防止重复发送，结果按 target/generation 回收并更新评论列表或错误状态。
- Git 状态刷新期间的 stage/unstage/hunk 操作会被显式门控，避免用户快速连续操作时与后台 status 任务并发写入同一 `.git/index.lock`；刷新完成后恢复操作。
- PR review 切换会级联失效评论列表和评论提交任务，即使 repository/number 相同也不会接受旧 generation 的网络结果。
- LSP Rename Workspace Edit 若资源冲突或预检失败，会保留 Rename 预览弹层并显示错误，避免用户失去冲突上下文后只能重新发起请求。
- LSP `workspace/executeCommand` 若服务端返回 Workspace Edit，会按当前 URI 执行并同步文档；非编辑结果继续按普通命令完成处理。
- `ttt.decorations` 已与插件 diagnostics 查询链分离：装饰不再伪装成 Info 诊断或污染状态栏消息，而是按 `style` 显示专用行号标记并保留正文下划线提示。
- 插件 diagnostics 与 decorations 现在使用独立存储，分别 clear 时不会误删另一类编辑器状态。
- Agent JSONL `tool_call` 缺少工具名称时会回写带原始请求 ID 的标准错误 `tool_result`，避免协议调用静默丢失。
- 脏缓冲区检测到磁盘外部变更后，普通保存与 `saveAllOnQuit` 均会阻止覆盖磁盘内容；用户需先显式重载或使用 Save As。
- Agent 工具 worker 在执行前再次校验会话 generation；Provider 切换或会话重启后，已排队但尚未开始的旧工具不会产生文件写入等副作用。
- LSP Workspace Edit 命中已打开的非活动 Buffer 时直接修改该内存 Buffer，不重新读取磁盘覆盖未保存内容；外部变更或只读目标会在预检阶段拒绝应用。
- Agent `write_file` 命中存在未保存编辑的打开 Buffer 时会拒绝覆盖磁盘；保存或重载后才允许写入，避免 Agent 写盘与编辑器脏状态发生静默冲突。
- 工作区 Replace All 对所有已打开标签统一作用于内存 Buffer；非活动脏标签不会被磁盘内容覆盖，替换结果可分别撤销或保存，磁盘基线保持安全。
- 编辑器匹配括号现在同时支持 `Ctrl-Alt-M` 跳转和正文可视高亮；导航与渲染复用同一跨行嵌套匹配算法，横向滚动和软换行下仍按字符列定位。
- LSP `formatOnSave` 现在等待异步格式化响应应用完成后再落盘；服务不可用、超时或返回错误时安全回退为普通保存，不会出现“先保存、后格式化又把 Buffer 变脏”的状态。
- 终端 DECRQM（`CSI ... $p`）现在准确报告常用 ANSI/DEC 模式，包括插入模式、132 列、反向视频、Origin、自动换行/重复、光标、鼠标和 bracketed paste 等状态；未知模式返回“不支持”。
- DECRQM 还会准确报告 `47`/`1047`/`1049` alternate-screen 状态，支持全屏程序在切屏生命周期中自检宿主终端模式。
- 编辑器自动缩进现在会在缩进空行中输入 `)`、`]`、`}` 时自动回退一个缩进级别，保留单次 undo 语义，并兼容空格与 Tab 缩进。
- `Buffer::apply_text_edits` 现在与其他编辑入口一致执行只读保护，LSP 格式化/代码操作、Workspace Edit 和插件批量粘贴不会绕过只读标签边界。
- 插件 Sidebar/Bottom panel、Drawer 和自定义 Tab 的聚焦 Input/Password 现在接收系统 Paste/bracketed paste，内容按控件状态路由，不会误落入编辑器 Buffer。
- 新增真实 Lua 插件集成回归：Sidebar 的普通 Input 与 Password 均能接收 `App::handle_paste` 内容并保持各自状态/掩码渲染。
- Drawer Password 已具备独立状态、Tab/Shift-Tab 焦点、键盘输入、系统粘贴和 `on_change` 回调；渲染始终使用掩码，且不会把密码文本写入插件面板输出。
- 终端 RIS（`ESC c`）会同时清理挂起的 bell 与 OSC52 剪贴板事件，避免 reset 前事件在新终端状态中延迟生效。
- Git inline conflict parser 遇到残缺或嵌套标记时不会跨标记错误配对，也不会因前一个残缺区块隐藏后续有效冲突区块。
- 终端 VT 相对光标移动：支持 `CSI n a` 水平相对移动和 `CSI n e` 垂直相对移动，并遵守 origin mode 与屏幕边界。
- 插件普通面板 `scrollview` 已复用 List 集合状态机，支持 `selected`、方向/Page 导航和 `on_select`，并纳入 Tab/Shift-Tab 焦点循环及焦点显示，不再仅静态渲染。
- 普通面板 Tabs 已纳入 Tab/Shift-Tab 焦点循环，支持焦点显示、鼠标聚焦、方向键切换和 Enter 激活。
- 普通面板 Radio 已纳入 Tab/Shift-Tab 焦点循环，支持焦点显示、鼠标聚焦、方向键切换和 Enter 激活当前选项。
- 普通面板 Dropdown 已纳入 Tab/Shift-Tab 焦点循环，支持焦点显示、鼠标聚焦、方向键切换和 Enter 选择回调。
- 重用已有编辑 tab 时会清理选区、多光标、折叠、水平滚动和 diff 临时状态，避免从旧文件带入瞬时编辑上下文。
- 搜索结果现在会合并所有已打开 tab 的未保存内存内容；搜索结果回车命中已打开文件时直接定位其现有 Buffer，不丢失未保存修改。
- 多标签搜索状态同步已补齐：打开、切换或关闭 tab 时重新合并当前未保存缓冲区，避免搜索结果残留上一个活动 tab 的内存匹配。
- 终端鼠标转发已按 DEC 模式过滤：Normal 仅转发按键/滚轮，Button 允许拖拽，Any 才转发裸移动，避免普通鼠标移动污染 PTY。
- 终端鼠标协议已支持 DEC 1005 UTF-8 与 1015 URXVT 坐标编码，且 `DECRQM` 可准确报告 1000/1002/1003/1005/1006/1015 鼠标模式。
- LSP 补全候选已保留原始 `data`，选择带数据的候选时先调用 `completionItem/resolve`，再应用解析后的 `textEdit`/`insertText`；服务不支持解析时安全回退原候选。
- LSP 补全还支持标准 `additionalTextEdits`，主编辑与附加编辑按原始 UTF-16 坐标从后向前应用，避免多范围替换发生位置漂移。
- LSP 补全解析器支持标准 `InsertReplaceEdit`，在同时存在 `insert`/`replace` 时使用 `replace` 范围替换已输入前缀。
- LSP `insertTextFormat=2` 补全 snippet 已做基础展开，支持默认值、选择列表和 `$0`/tabstop 清理，避免模板标记直接写入缓冲区。
- LSP `completionItem/resolve` 现在会把返回的 detail/documentation 与原始候选合并，保留原候选的编辑范围和附加编辑，避免服务端只返回元数据时错误退回 label。
- LSP snippet 展开支持多位 tabstop 编号（如 `${10:value}` 和 `$10`），并继续清理默认值与选择列表占位符。
- LSP 补全支持 `commitCharacters`，输入候选声明的单字符时自动提交候选、插入该字符并同步文档。
- Explorer 文件/目录重命名后会迁移所有受影响的已打开 Buffer 路径，并同步迁移语言覆盖配置键，保持标签、保存目标、重启后的语言识别和后续 LSP URI 与磁盘路径一致。
- 活动文件重命名时会先向 LSP 发送旧 URI 的 `didClose`，迁移后按新 URI 重新同步，避免语言服务器保留旧文件缓存。
- LSP 服务发生请求/通知写入失败、超时或进程退出时会统一清理活动客户端、失败状态和文档版本；重新启动服务后首个同步稳定从 `didOpen` 开始，避免向新进程发送过期 `didChange`。
- LSP 文档关闭或重命名时同步清理旧 URI 的 diagnostics，避免关闭文件后的过期错误继续显示在状态栏和行号区。
- LSP Rename/Workspace Edit 预览会按 `overwrite`、缺失源文件、删除目标缺失等资源操作语义标注冲突，使 Enter 应用前即可识别不可安全执行的变更。
- 自定义插件 Tab 在关闭或同 ID 重开时会释放旧 render、on_event 和 on_close registry，避免回调泄漏及旧视图残留。
- 自定义插件 Tab 关闭时还会清理选择器、按钮、标题菜单、输入框、密码框和复选框等面板运行态，防止关闭后的回调再次被触发。
- 关闭活动编辑 Tab 或自定义插件 Tab 后，Host 会在 `tab.close` 后重新派发 `tab.activate`，让插件及时获得新的活动上下文。
- 关闭普通编辑 Tab 时补齐 `document.close`/`file.close` 生命周期事件，并在 `tab.close` 前携带关闭文件路径，兼容声明 `events.file` 的原版插件。
- 新增生命周期回归测试，验证关闭活动编辑 Tab 时插件收到 `tab.close` 后再收到新活动文件的 `tab.activate`，事件顺序稳定。
- 回归测试同时覆盖 `require("ttt.events")` 订阅入口，确认其生命周期事件行为与 `ttt.on` 保持一致。
- Host 直接关闭插件文件视图时也统一派发 `tab.close` 后的 `tab.activate`，并由回归测试覆盖普通编辑 Tab 与自定义插件 Tab 两条路径。
- Agent 结构化文件工具增加 4 MiB 的读写上限，`list_files` 返回确定性排序；workspace canonical 边界和符号链接不跟随策略保持不变，并覆盖大文件回归测试。
- Agent `write_file` 临时文件改为独占创建，拒绝预先存在的符号链接，避免原子替换前被链接劫持到 workspace 外；Unix 回归测试覆盖该攻击路径。
- 插件 `panel:progress` 降级渲染现在读取当前 panel 内容区宽度生成进度条，兼容原版“填充面板宽度”语义，并保留窄窗口安全降级。
- `panel:progress` 默认填充字符已对齐原版为 `▄`，显式 `char` 仍按插件配置使用。
- 目录重命名时，LSP `didClose` 使用受影响活动文件的真实旧 URI，而不是被重命名的目录 URI。
- LSP Workspace Edit 的 `RenameFile` 现在复用工作区路径迁移逻辑，同步已打开 Buffer 与语言覆盖配置，避免资源操作后标签指向旧路径。
- LSP `RenameFile` 不再拒绝活动文件作为源，活动 Buffer 会完成旧 URI 关闭、磁盘重命名和新路径迁移。
- Git 分支切换后会立即重载干净的已打开 Buffer，脏 Buffer 保留内存内容并提示磁盘变化；任一标签重载都会刷新搜索，活动文件重载后额外同步 LSP 和插件文档事件。
- Buffer 外部文件删除现在会产生明确且去重的 Deleted 状态；UI 提示“save to recreate or close tab”，不会静默把旧内容当作仍存在于磁盘。
- 插件 `editor.write` 的选区、多光标、清除选区和 diff 跳转请求现在会触发 `cursor.change`，即使主光标位置未变化也不会丢失选择状态事件。
- 插件 `editor.write` 请求应用后已统一同步 LSP、刷新搜索结果并派发 `editor.change`；仅光标/选择变化时派发 `cursor.change`，避免插件写入绕过宿主事件链。
- 终端 alternate screen 快照已纳入 bracketed paste、鼠标跟踪和 SGR 鼠标编码模式，切回主屏后恢复宿主原有输入状态。
- 终端 alternate screen 快照已纳入 DECCKM application cursor keys，切回主屏后恢复主屏键盘模式，避免方向键编码被 alternate screen 污染。
- 终端 alternate screen 快照已保存/恢复当前 SGR 样式与光标可见状态，避免程序切回主屏后继承 alternate screen 的显示模式。
- 插件 Drawer 已支持 input/checkbox/dropdown/button 的 Tab/Shift-Tab 双向焦点循环；按钮焦点下 Enter/空格可重复触发回调，重绘时仍统一清理 Lua 资源。
- Bottom 插件面板鼠标事件顺序已与 Sidebar 统一：先处理控件和行内 action，再处理行选择，避免带 `on_select` 的行截获 action 点击。
- 普通面板控件焦点已互斥：鼠标点击 input/password/checkbox/button 会清除其他控件焦点，避免后续按键被旧控件误消费。
- 普通面板 input/password/checkbox/button 已支持 Tab/Shift-Tab 双向焦点循环，焦点切换后可继续使用原有键盘激活和输入行为。
- 普通面板密码控件支持受控状态、鼠标聚焦、字符输入、退格、实时 `on_change` 回调和等长度掩码渲染。

- 插件普通面板 Checkbox 已支持鼠标聚焦切换、Enter/空格切换、`on_change(checked)` 回调和跨重绘状态保留。

- 普通面板按钮支持键盘激活：鼠标点击后保留按钮焦点，Enter/空格可重复触发 `on_click`，直到面板重绘或插件卸载。

- 普通面板按钮回调改为可重复激活：同一 `panel:button` 可连续点击，只有面板重绘或插件卸载时才释放 Lua callback。

- 插件 List/Table 行内 `actions` 已建立按 Unicode 列宽计算的鼠标命中区域，点击后复用 `on_command(command, node|row)`，并在面板重绘时重建区域状态。

- 插件普通 Sidebar/Bottom/自定义 Tab 的 `panel:input` 已支持鼠标聚焦、字符输入、退格、Enter 提交、`on_change`/`on_submit` 回调和跨重绘文本保留，状态按面板隔离。

- 插件普通 Sidebar/Bottom/自定义 Tab 的 `panel:button({on_click=...})` 已支持鼠标激活，并在面板重绘、插件关闭时清理 Lua 回调资源；Dialog/Drawer 行为保持兼容。

- 终端 VT BEL（`\x07`）已转换为一次性事件：主循环消费后在状态栏提示 `Terminal bell`，连续 BEL 会合并为一次待处理提示，不污染终端内容。

- 插件 Tree/List/Table 已支持控件级 `inset`：行内容按配置增加前置空格，并在截断和 badge 右对齐计算中保留布局空间。

- 插件 Tree/List 的 `badge_right=true` 已按 TTT 配置语义实现：行内 `badge` 按面板终端列宽右对齐，空间不足时保留主体文本并安全截断 badge。

- 插件 Tree/List 的 `truncate_left` 已按实际面板终端列宽实现，使用 Unicode display width 保留长标签尾部，并覆盖中英文混排回归测试。

- 插件 Tree/List 的键盘上下文菜单已对齐 TTT：聚焦面板中按 Shift+Enter 会针对当前选中节点打开 `node_menu`，菜单命令继续以当前节点传给 `on_command`。

- 插件面板 Mouse Event：Sidebar/Bottom 面板 `on_event` 现在可接收 0-based `x/y` 与 `left/right/middle/wheel_*` 按钮事件；返回 `true` 时消费鼠标，否则继续执行原有控件和面板焦点逻辑。

- 插件 Tabs 状态兼容：支持 TTT 的字符串 `active=id` 与旧数字 `selected` 双向同步，重绘间保持选中标签并以 `on_change(tab_id)` 派发切换回调。

- 插件 Pager 交互：兼容 TTT 的 `offset/limit/total` 分页模型，保存页状态并支持 Left/Right、上下、PageUp/PageDown、Home/End 翻页回调 `on_change(new_offset)`，同时兼容旧 `page/pages` 字段；Sidebar、Bottom、Drawer 和自定义 Tab 共用该实现。

- LSP 窗口消息：标准 `window/showMessage` 与 `window/logMessage` 通知现在会显示在状态栏，保留服务器的重要提示并区分普通消息与日志。

- Terminal/Agent alternate screen 输入闭环：Escape 直接发送至 PTY，F1–F12 映射为标准 VT 功能键序列，避免被宿主导航逻辑吞掉。

- Agent alternate screen 按键路由：独立 Agent PTY 现在与 Terminal 一致支持 Home/End、Insert/Delete、PageUp/PageDown，并按 Agent 自身的 application cursor mode 编码方向键。

- 终端 alternate screen 按键路由：Home/End、Insert/Delete、PageUp/PageDown 现在向 PTY 发送标准 VT 序列；普通终端仍保留 scrollback 翻页，方向键继续区分 application cursor mode。

- LSP 服务端 Workspace Edit：Host 现在接收语言服务器主动发起的 `workspace/applyEdit` 请求，复用现有 Workspace Edit 预检/应用逻辑，并向服务端返回 `applied`、变更数量或 `failureReason`。

- 插件事件容错：同一事件的多个 Lua 回调现在会在单个回调异常后继续派发，派发结束后报告首个错误；新增回归测试确认后续监听器仍执行。

- 插件面板尺寸闭环：`panel:size()` 现在也会在 Drawer 与自定义编辑器 Tab 的实际内容区重绘前同步宽高；Raw Cell/自适应插件无需再依赖默认尺寸，Git diff Tab 仍保持原有渲染路径。

- LSP 补全范围编辑：补全项支持 `textEdit.range`，按 UTF-16 协议列转换为编辑器 Unicode 列后替换指定范围；新增多字节文本替换回归测试。

- LSP 补全插入文本：补全弹层继续显示 `label`，Enter 插入时优先使用协议提供的 `insertText` 或 `textEdit.newText`，避免泛化为展示标签。

- LSP DeleteFile：删除活动文件时发送旧 URI 的 `didClose`，将打开 Buffer 标记为 Deleted，并避免使用旧 URI 立即重新同步；用户可保存以重建文件或关闭标签。

- 插件 Button 兼容：`button({disabled=true})` 仍渲染按钮标签，但不会注册点击动作、抢占焦点或响应 Enter/Space，适合请求进行中等不可操作状态。

- 插件受控 Input：`input({value=...})` 会在每次重绘同步外部值且不触发 `on_change`；同时提供 `text` 与 `value` 时以受控 `value` 为准，省略 `value` 时保留宿主内部编辑状态。

- 插件 Input 提交：`clear_on_submit=true` 已覆盖 Sidebar、Drawer 和 Dialog 输入；提交回调收到原始文本后，Host 清空内部输入状态并等待下一次重绘。

- 插件 Markdown：`panel:markdown` 支持字符串和 `{text=...}` 表格两种调用形式，按行展开标题、粗体/斜体/代码 span，并将 Markdown 链接显示为可读标签；fenced code 围栏及代码原文会保持不变。

- 插件 Label：`panel:label` 支持字符串和 `{text=..., badge=...}` 配置表形式，Sidebar/Bottom/Drawer/自定义文件视图均可直接使用。

- 插件 Input 前缀：`input({prefix="..."})` 只影响显示，不会写入输入值或提交/变更回调参数，已覆盖 Sidebar、Bottom、Drawer、Dialog 和文件视图。

- 插件 Title：`panel:title` 支持字符串和 `{text=..., badge=...}` 配置表形式，主面板与插件文件视图保持一致。

- 插件 Title 菜单：`panel:title({menu=..., on_menu=...})` 已接入宿主上下文菜单状态机，支持主面板、自定义 Tab 与文件视图，包含 separator/checked 文本、键盘选择和命令回调。

- 终端/Agent alternate screen：`Shift-Tab` 与 `BackTab` 现在发送标准 VT back-tab 序列，不会误触发 mtide 宿主焦点切换。

- 插件 Input 自动焦点：`autofocus=true` 已在面板获得焦点、打开/聚焦自定义 Tab 时自动选中对应输入控件。

- 插件编辑器视口：`ttt.editor.viewport()` 新增 `height` 和准确的 `bottom_line`，Host 在 UI 绘制时同步编辑区内容高度，插件可据此进行可靠的可见范围计算。

- 插件安装生命周期：新增 `ttt.on_install(callback)` 一次性安装钩子；普通加载不触发，Registry 安装成功后执行，异常会报告安装失败并删除本次安装目录，覆盖一次性执行回归测试。

- 插件 Raw Cell 尺寸：新增 `panel:size()`，Sidebar/Bottom 渲染时传入当前 Ratatui 内容区宽高，Raw Cell 插件可以根据实际布局自适应绘制，并新增尺寸读取回归测试。

- 插件 Diff 导航：`ttt.editor.diff()` 可读取活动可编辑 Diff 的变更块，`editor.go_to_diff_hunk(index)` 通过 Host 请求跳转并滚动到指定 hunk，新增 Lua API 与请求队列回归测试。

- 插件 Raw Cell 坐标绘制：`panel:text(x, y, text)`、`panel:cell(x, y, char)` 现在保留坐标、支持多行文本和空白填充；`panel:clear(x, y, w, h)` 会清理对应二维区域，新增位置与清理回归测试。

- 插件书签交互：`gutter.click(path, line)` 已在编辑器行号左侧书签列接入，按 `editor.bookmarks` 权限派发；无订阅时保留原有编辑器行为，并新增事件位置回归测试。

- LSP Code Action 命令动作：Code Action 不再只接受工作区编辑；标准 `command`/`arguments` 动作会进入统一选择流程，并调用 `workspace/executeCommand`，新增命令变体规范化测试。

- 普通面板控件回归隔离：新增测试确认 Sidebar/Bottom/Tab 面板中的 button/input/checkbox/dropdown 不会注册 Drawer 专属状态，也不会误消费 Drawer 输入、焦点或激活事件。

- 插件控件状态隔离：Drawer 专属 button/input/checkbox/dropdown 状态仅在 `__drawer` 渲染上下文注册，普通 Sidebar/Bottom/Tab 面板不会误占用 Drawer 回调或输入状态。

- 插件 Table 命令：支持 `key_commands` 快捷键映射，并将当前 1-based 行号与命令传递给 `on_command(command, row_index)`，覆盖键盘操作路径。

- 插件 Raw Cell API：panel proxy 新增 `clear(x, y, width, height)` 兼容入口，允许上游插件安全清理自定义绘制区域。

- 插件 Drawer 兼容补齐：panel proxy 新增 `keyvalue`、`divider`、容器 `render` 子内容和顶层坐标式 `text/cell`，Drawer button 的 `on_click`、input 的 `on_change`/`on_submit`、checkbox 状态切换与 dropdown 选择已接入鼠标/键盘交互，覆盖上游 `ttt-mcp` 的 `mcp.drawer` 渲染路径。

- 插件事件派发稳定性：`ttt.events` 和 `key.press` 会在调用 Lua 前快照回调函数，回调中新增/修改订阅不会触发借用冲突；新增回归测试覆盖同一事件派发期间动态注册监听器。

- 插件输入回调容错：Sidebar/Bottom/Drawer、自定义标签、文件视图和命令行的 `on_event`/`on_key` Lua 异常会记录到插件日志并按未消费处理，避免单个故障插件阻塞宿主输入。

- 插件事件 payload：`editor.change`/`cursor.change` 现在附带当前行文本、文档行数和 dirty 状态，并保持旧版回调参数兼容。
- 插件交互弹层：`ttt.confirm`、`ttt.show_info`，包含 overlay 焦点恢复、Enter/Esc 操作和确认回调。
- 插件命令行：`ttt.command_line.show/hide/set_text/active`，支持 prefix、初始文本、Unicode 光标编辑、实时 `on_change`、提交/取消回调和可消费的 `on_key`，并接入 overlay 焦点恢复。
- 插件定时辅助：`ttt.debounce` 已支持参数透传、重复调用合并和取消，并复用 Host 主循环 timer 调度。
- 插件 Markdown 辅助：`ttt.markdown` 已返回按行/span 分组的纯 Lua 结构，覆盖标题、粗体、斜体、行内代码和普通文本。
- 宿主信息：`ttt.executable_path()` 已返回当前 mtide 可执行文件绝对路径。
- 插件 Changes 标签：`ttt.open_changes` 已校验 unified diff 文件头并创建可渲染的只读 tab，支持 id/title/description/file_path，以及滚动、Home/End、`[`/`]` hunk 导航、`c` hunk 折叠、`v` unified/split 视图切换和关联文件 Enter 编辑。
- 插件通用弹层：`ttt.open_dialog`/`ttt.close_dialog` 已接入 title、尺寸、panel proxy render、基础 input 字符/退格/on_change、input/dropdown/button Tab 焦点循环、dropdown 方向键/Enter/on_change、button Enter/按内容行鼠标命中/on_click、Esc/主动关闭和 `on_close` 生命周期回调；复杂的自定义布局仍按兼容降级文本渲染。
- 插件网络：`ttt.net` 已支持 HTTP(S) GET/POST 同步与异步请求、权限校验、30 秒超时、10 MiB 响应上限、主线程 Lua 回调，以及 `network.http` 主机/IP/可选端口 allowlist。
- 插件进程：`ttt.system.spawn` 已支持受 `system.exec` 权限保护的长生命周期子进程、stdout/stderr 流式回调、退出码回调，以及 `write`、`close_stdin`、`kill`、`pid` 句柄操作；Host 会等待 stdout/stderr 流关闭后再释放回调注册，避免子进程快速退出时丢失尾部输出；所有 Lua 回调均在 Host 主循环执行。
- LSP 保存生命周期：已在普通保存、Save As 和退出批量保存后发送 `textDocument/didSave`，并覆盖发送失败时的客户端失败状态与协议结构测试。
- 终端宽字符清理：覆盖从 continuation 单元擦除宽字符时的全单元样式、组合字符和 OSC 8 链接元数据清理，避免残留渲染状态。
- 终端组合字符清理：覆盖组合标记随基字符擦除、退格覆盖和宽字符边界操作的一致性回归。
- 终端插入模式：支持 CSI `4h/4l` 插入模式，并在 alternate screen 切换时保存/恢复该状态。
- 终端自动换行：支持 DEC `CSI ?7 h/l` 自动换行模式，并在 alternate screen 切换时保存/恢复该状态。
- 终端 alternate screen 兼容：兼容 DEC `CSI ?47 h/l`、`?1047 h/l` 和 `?1049 h/l` 三种切换别名。
- 终端 origin mode：支持 DEC `CSI ?6 h/l`，滚动区域内的光标寻址可切换局部原点并正确恢复。
- 终端私有光标状态：支持 `CSI ?1048 h/l` 独立保存/恢复光标位置。
- 终端 soft reset：支持 DEC `CSI ! p`，重置交互模式与样式而保留当前屏幕内容。
- 终端制表位：支持默认制表位、`ESC H` 设置当前制表位、`CSI g` 清除当前/全部制表位；resize 保留已有自定义制表位，并为新增列补充默认布局。
- 终端组合字符光标边界：CSI 光标定位和水平移动会避开宽字符 continuation 单元，避免组合字符/宽字符附着到错误单元。
- 终端 DEC 列模式：支持 `CSI ?3 h/l` 在 132/80 列间切换，并按终端约定清空切换后的屏幕。
- 终端 DEC 反色模式：支持 `CSI ?5 h/l`，渲染、alternate screen 快照和 soft reset 均保持一致。
- 终端 DEC 自动重复模式：支持 `CSI ?8 h/l` 状态跟踪，并在 alternate screen 和 soft reset 时保持正确恢复/重置。
- 插件 Dialog 导航：支持多个按钮的 Tab/Shift-Tab 双向焦点循环，并只激活当前焦点控件。
- 插件社区 registry：新增 HTTPS-only registry index 读取、按名称/版本选择（未指定版本时取最高数字点号版本）、受大小/超时/重定向限制的插件包下载、安全 ZIP 安装和更新失败回滚接口，并校验插件名称、版本、下载 URL、可选 SHA-256 及归档成员路径；命令面板新增 `plugin.refreshRegistry`，从 `MTIDE_PLUGIN_REGISTRY` 读取 index 并在 Output 显示条目，Output 中按 `i` 可安装并立即加载当前条目到 workspace `.mtide/plugins`。
- 插件 Dialog 资源清理：多次重绘时清理旧 input/button Lua 回调注册，避免重复刷新累积 registry 资源。
- 插件 Dialog 关闭清理：关闭时移除 widget 状态及全部 Lua 回调注册，避免关闭后残留可调用控件。
- 插件密码控件：`panel:password` 独立渲染输入值，仅显示等长度掩码；空值仍保留 placeholder，避免通用控件路径泄露明文。
- 插件表格水平滚动：`table` 支持 `scroll_x`/`horizontal_scroll` 和可选 `width`，按 Unicode 字符窗口裁剪行内容并覆盖回归测试。
- 插件 Table 列模型：支持原版 `columns` 的标题、固定 `width`、自动列宽、`min_width`、left/center/right 对齐；同时兼容数组行和 `{cells = {...}}` 行，未声明 `columns` 的旧插件继续使用原有降级文本渲染。
- 插件 Table 列 actions：动作列会按列宽显示动作标签，并建立按 Unicode 列宽计算的鼠标命中区域，点击后向 `on_command(command, row_index)` 派发命令。
- 插件 Table 加载态：`loading=true` 且已有 rows 时保留当前数据并在列标题行显示加载提示；无 rows 时继续显示独立 loading 状态。
- 插件 Table 横向滚动：带 `actions` 的末列固定在右侧，横向滚动只裁剪数据列，并保留动作标签的 Unicode 列宽与鼠标命中区域。
- 插件 Table 固定列：显式 `width` 列不会被超长单元格撑大，内容按终端列宽截断；未指定宽度的列继续按 `min_width` 与内容自动布局。
- 插件弹层鼠标交互：自定义 dialog 会按内容行定位 input、dropdown 和 button，鼠标点击可切换焦点并触发对应控件动作；键盘导航和关闭焦点恢复保持不变。
- 插件 Tab 生命周期清理：自定义 tab/file view 的 `on_close` 回调执行后删除 registry key，避免重复打开关闭累积 Lua 资源。
- 插件只读/差异标签资源上下文：`ttt.open_readonly` 与 `ttt.open_diff` 的可选 `file_path` 已随 Host 的 `OpenTab` 请求保留，供标签生命周期关联资源。
- 默认工作区启动：无显式 target 时聚合全部 `defaultFolders`/`startupFolders` 为多根 workspace，并支持 `~/` 展开与目录校验。
- 默认工作区清除：新增 `--clear-default-folders`，清空并持久化当前配置的默认启动目录。
- GitHub PR target：已增加 GitHub Pull Request URL 的安全解析与仓库/编号校验；远程 fetch、review diff、评论加载/提交和按评论定位已接入。
- Git PR fetch service：Git backend 已提供校验后的 PR head fetch 到 `refs/mtide/pr/<number>`，不切换当前分支；Review diff 展示、评论加载/提交和按评论定位已接入。
- Git PR diff service：Git backend 已提供基于任意两个 ref 的安全 unified diff 读取，并复用现有 hunk parser；Review UI 已接入 Output 视图、评论加载/提交和按评论定位。
- Git PR App 入口：App 已提供 fetch PR head、读取 `HEAD..PR ref` diff 并切换 Output review 视图的入口，不切换当前分支；GitHub PR URL 已路由到该入口，Output 支持加载评论、提交当前 hunk 评论并按 Enter 跳转到评论文件行。
- GitHub PR review comment backend：Git backend 已提供受 GitHub 仓库校验、30 秒超时和环境令牌保护的评论列表/行级评论创建接口，令牌不写入日志或配置；Output review 视图可用 `m` 拉取并显示评论，`c` 可针对当前 hunk 输入并提交右侧行级评论，Enter 可定位到评论文件行。
- 插件 Diff 标签导航：`ttt.open_changes` 自定义标签已接入滚动位置和 hunk 位置状态，支持键盘导航、关闭和回归测试。
- 插件 tab 生命周期：`ttt.open_tab` 支持 `on_close`，Host 关闭、替换或插件主动关闭自定义标签时会执行回调并移除注册资源。
- LSP 文档关闭生命周期：编辑器关闭标签或退出前发送 `textDocument/didClose`，并移除对应文档版本；发送失败会将语言服务器标记为失败。
- Git 远程认证：fetch/pull/push/sync 认证失败后提供内存凭据 overlay 和重试路径，凭据通过一次性 Git credential helper 传递，不写入磁盘；sync 的 pull 失败时不会执行 push。
- CLI 自动化：`--exec` 已接入启动阶段 JSON 动作执行，支持 `key`、`type`、`paste`、`wait` 和 `quit`，并对动作格式、按键和等待上限做校验；`--exec-split-on` 可拆分多个 JSON 脚本顺序执行。
- 工作区 Quick Open：Open File overlay 已支持多根 workspace 递归文件发现、模糊路径筛选、上下选择和 Enter 打开；默认跳过隐藏目录以及 `target`/`node_modules`/`vendor` 构建目录，Save As 仍保留直接路径输入。
- Quick Open 命令模式：输入 `>` 可在同一 overlay 中切换到内置/插件命令过滤，支持上下选择、Enter 执行和 Esc 取消；Ctrl-P 独立命令面板保持兼容。
- Quick Open 帮助模式：输入 `?` 可在同一 overlay 中浏览并筛选核心快捷键说明，支持上下移动、Enter/Esc 关闭。
- 终端快捷键兼容：`Ctrl-K T` 会创建并聚焦新的终端标签，`Ctrl-K` chord 在终端/Agent 原始输入焦点下不拦截 PTY 按键。
- Quick Open 配置兼容：支持 `quickOpen.showHidden` 与 `quickOpen.showGitIgnored`，分别控制隐藏路径和 Git ignore 路径的发现；Git 仓库通过 `git check-ignore --no-index` 判断，非 Git workspace 安全降级。
- Explorer 配置兼容：支持独立的 `explorer.showHidden` 与 `explorer.showGitIgnored`，目录展开、刷新和多根 workspace 均沿用相同过滤规则。
- Explorer Git 状态展示：文件树显示 index/worktree 双字符状态，并按新增、修改、删除和冲突状态使用主题颜色。
- Explorer 自动 reveal：支持 `explorer.autoReveal`，打开或切换活动文件时自动展开所属根目录及父目录并定位当前文件。
- 运行中工作区扩展：复用窗口收到新的目录目标时会将其加入多根 workspace，立即刷新 Explorer、跨根搜索和 Quick Open；重复目录请求幂等处理。
- 多根 Git Explorer 状态：按 workspace root 独立缓存 Git 状态，文件树会解析所属根目录并展示对应 index/worktree 标记；现有首根 Changes 操作保持兼容。
- 多根 Changes 聚合：Changes 面板聚合所有 workspace root 的变更，显示根目录前缀；打开、diff、stage/unstage、hunk 和冲突处理均按变更所属根目录执行。
- 多根 Git 分支上下文：状态栏和 Branches 面板跟随活动文件所属 workspace root，分支创建/切换/删除及 fetch/pull/push 使用对应仓库。
- 多根 Git 历史操作：commit、blame 和 history 同样跟随活动文件所属仓库，不再固定使用首个 workspace root。
- Git Sync：Changes 面板和命令面板支持后台 `sync`（`pull --ff-only` 成功后 `push`），pull 失败时不会执行 push，并沿用远程认证重试流程。
- 多根 LSP 上下文：活动文件切换或服务重启时，LSP `rootUri` 使用所属 workspace root，避免第二个项目复用首根工程上下文。
- 多根 LSP 会话隔离：同一语言切换到不同 workspace root 时会关闭旧客户端、清理文档版本并按新 `rootUri` 启动独立会话；相同根目录复用现有会话，异常退出与宿主关闭会同步清理 root 绑定。
- Settings overlay 已提供 Quick Open/Explorer 四个可见性开关，切换后立即刷新对应文件列表，并可用 Ctrl-S 持久化。

