# 当前实现状态

本文记录代码仓库当前真实状态，和产品需求/迁移计划中的目标状态分开维护。

## 已落地

- Ratatui/Crossterm 全屏布局：菜单栏、Explorer/Find/Changes 侧栏、编辑器、Terminal/Output 底栏、AI Agent 区、状态栏、编辑器自动垂直滚动、鼠标捕获、bracketed paste 粘贴事件路由/编辑区定位和窄窗口降级布局；Open File、插件命令面板和 LSP 弹层使用 overlay focus 栈，取消后恢复打开前焦点。
- CLI 自动化：`--size WxH` 会在启动时调整 PTY/Agent 尺寸并不会污染位置目标；`--exec` 已支持键盘、文本/粘贴、复制、真实鼠标 click/rclick/hover/drag、panel 焦点、文件 open/定位/只读、debug/screenshot 输出和基于文件存在的有界 wait-for；默认实例通过当前用户/目录对应的 loopback Unix socket 接收后续进程的文件目标和行列位置，`--new-window` 可绕过复用，`--listen` 显式声明监听实例。
- 键盘导航：Tab/Shift-Tab 焦点循环、面板切换、侧栏/底栏标签切换、退出快捷键。
- 文本编辑：UTF-8 字符列、插入/换行/删除、上下左右/Home/End、`Ctrl-G` 行列定位、`Ctrl-Backspace`/`Ctrl-Delete` 按词删除（含跨行边界和单次撤销）、保存规范化、Save As 新路径保存与覆盖保护、自动缩进、可选 format-on-save、语言覆盖、`saveAllOnQuit` 批量保存、撤销/重做、dirty 状态、括号/引号自动闭合和闭合符跳过、基于缩进的代码折叠/全部展开、当前文档 Replace All 单批次撤销、重叠多选区合并、复制行/删除行/按已知语言切换注释（未知语言会明确提示不可用），以及 Shift 选区、支持按光标/选区逐项 spread 的多光标剪贴板、内部剪贴板和受 64 KiB 限制的 OSC52 外部剪贴板；编辑器支持多标签，每个标签独立保留缓冲区、光标、撤销栈和 dirty 状态。搜索 Replace 模式下按 `Ctrl-Shift-Enter` 可对当前搜索结果覆盖的工作区文件执行全部替换，并遵循与搜索一致的 smart-case 规则；当前打开文件保留内存 dirty/undo 语义，其他文件直接写回磁盘；脏文件检测到磁盘变更后可用 `Ctrl-Alt-R` 显式重载。
- 工作区：目录浏览、文件打开、Explorer 新建文件/目录、重命名、删除确认和刷新、递归 smart-case 搜索、Unicode 字符列结果、可配置 include/exclude glob、编辑器 `Ctrl-F` 继承单行选区、活动缓冲区未保存编辑后的实时搜索结果、跨根结果循环导航和搜索结果跳转、当前搜索结果跨文件 Replace All、`.ttt` 多根 folder 聚合搜索/Explorer、打开文件磁盘 mtime 轮询与干净标签自动重载、脏标签外部变更保护、Git 分支/status、Branches 侧栏创建/切换/删除分支、unstaged/staged unified diff 读取、保存后状态刷新，以及 Changes 面板整文件/单 hunk/批量 stage/unstage、提交消息提交、当前文件 blame、提交历史和后台 fetch/pull/push 操作；远程结果在主循环回收，认证失败进入内存凭据重试 overlay；Changes 面板按 `d` 可进入可滚动 Output diff 视图。
- 终端：portable-pty 多 tab 持久 shell、settings shell/args/scrollback/ctrlV 配置、后台读取、命令提交、方向键、application cursor mode、PageUp/PageDown 与鼠标滚轮 scrollback 浏览、Tab、Ctrl-C/Ctrl-D/Ctrl-L、可选 Ctrl-V 剪贴板写入、resize，以及 VT screen（光标、滚屏、独立 scrollback、宽字符双列占位、组合字符附着、alternate screen、CSI 移动/清屏、SGR 基础颜色/粗体/下划线/反色），并在底栏按 ANSI 样式渲染；Terminal 焦点下支持 `Ctrl-Shift-T` 创建和 `Ctrl-Alt-Left/Right` 切换；解析终端的 1000/1002/1003/1006 鼠标模式，并按 SGR 或 legacy X10 编码转发鼠标事件至 PTY。
- LSP：配置模型、按语言选择 server、后台 stdio JSON-RPC reader、request/response 匹配、initialize、initialized、didOpen、按 URI 递增版本的 full didChange、shutdown/exit、事件循环轮询，以及 publishDiagnostics 状态解析和编辑器行号标记、UTF-16 code unit 列与编辑器字符列双向转换、completion/hover/signatureHelp/definition、workspace edit Rename、formatting 和首个 Code Action。
- TTT 插件基础运行时：manifest 校验、Lua 5.4、`ttt`/`ttt.json`/`ttt.system`/`ttt.fs`/`ttt.text` 模块、权限拒绝、包目录路径约束，以及 commands（`handler`/旧 `run`）/keybindings/sidebar/bottom/file_handlers 注册、兼容 panel proxy、list/table 的 selected/loading/empty 状态、同步/回调式 `system.exec` 与 `exec_async`；扩展名匹配的 file handler 可执行 `open(path, tab)` 并把 `tab.render`/字符串结果显示为插件文件视图，`tab.on_close` 生命周期回调可在视图关闭时执行并清理资源；`tab.open`、`tab.activate`、`tab.close` 覆盖普通编辑 tab 与插件文件视图的生命周期。
- 配置：`MTIDE_CONFIG_DIR`、`TTT_CONFIG_DIR` 和项目 `.mtide/settings.json` 的基础布局配置；支持 `defaultFolders`/`default_folders`/`startupFolders` 无显式目标时启动默认工作区；启动时读取工作区 `.editorconfig` 的常用缩进/保存规则；Agent 兼容原版 `agents` 顶层 schema，支持 `cli`、字符串/数组 `args`、`install`，并归一化为内部 provider 模型；`autoClosingBrackets`、`selectionStyle`、`reuseWindow` 等兼容设置已接入，未知顶层和 editor 字段会在保存/序列化时保留。
- 设置 UI：命令面板中的 `Open Settings` 可打开键盘导航的设置覆盖层，支持即时切换侧栏、底部面板、Agent、软换行、保存格式化、退出批量保存、终端 Ctrl-V 和 LSP 开关；当前值作用于本次会话，持久化编辑仍通过 settings 文件完成。
- CLI：支持 `--help`、`--version/-v`、文件/目录位置参数、`file:line:column` 定位、`.ttt` 多根工作区解析、`--config FILE`、可重复的 `--plugin DIR`、`--listen`、`--new-window`、`--welcome` 和标准输入目标；无目标时自动检测 pipe/regular file，显式 `-` 可强制读取，标准输入均在进入 raw mode 前读入并作为未保存 tab 打开；显式文件不可读或 `.ttt` 工作区解析失败时回退欢迎缓冲区/当前目录并在状态栏保留错误，编辑器支持 `Ctrl-O` Open File 路径弹层。

## 当前兼容边界

以下能力已经在需求文档中定义，但尚未达到完整产品级实现：

- 编辑器多标签、标签切换和未保存关闭保护已接入；每个 tab 可用 `Ctrl-Alt-L` 单独循环语言覆盖，覆盖会保存到工作区 `.mtide/language-overrides.json` 并在重启/重新打开时恢复，立即驱动状态栏、注释、语法着色和 LSP server 选择；已支持关闭 soft-wrap 时的水平视口和光标可见性、按终端列宽计算的 `editor.wordWrap`/`editor.wordWrapMaxColumn` 软换行，以及软换行开启时按屏幕行移动 Up/Down（含宽字符列位置保持）；还支持常见语言轻量语法着色、Markdown `<details>` 区块折叠（忽略行内代码和 fenced code 中的字面量标签）、鼠标点击/拖动选择、`Alt+Shift` 矩形/列选区、`Ctrl-A` 全选、`Ctrl-Alt-N` 选择下一个相同文本、`Alt-Shift-I` 跨行选区拆分为逐行 selections、缩进代码折叠、`Alt-Shift-Up/Down` 原子化移动行、`Ctrl-J` 原子化合并行、`Alt-Shift-S` 排序行和 `Alt-Shift-U/L` 大小写转换，以及 `Alt-Up/Down`/Alt-click 附加光标和多光标字符/换行/退格/删除；列选区已接入复制、剪切、替换和单次撤销，尚无完整格式化和增量语法高亮引擎。
- 终端已具备 PTY 字节通道、跨读取边界的 UTF-8 解码、ANSI 样式（基础色、亮色、256 色、真彩色、粗体、faint、italic、strikethrough、conceal、blink，RGB 参数安全截断到 0–255）、光标可见性控制（`CSI ?25 h/l` 以反色渲染当前光标单元）、DECSCUSR `CSI Ps SP q` 光标样式状态（块/下划线/竖线）及 alternate-screen/soft-reset 快照语义、DEC bracketed paste 状态跟踪及标准粘贴包装、alternate screen 下字符/退格/回车/Tab 和 ASCII 控制组合直通 PTY、宽字符 continuation 元数据及按字符退格/覆盖/清除、alternate screen（含主屏 links/样式/光标/滚动区域快照恢复）、独立 scrollback、基础鼠标协议转发（SGR/X10）、OSC 8 超链接点击打开（包括 scrollback 浏览中的可视行）、组合字符附着、保存/恢复光标、`CSI r` 局部滚动区域、`CSI L/M/S/T` 区域行编辑与滚动、IND/RI/NEL、CSI Tab/BackTab、DSR `CSI 5 n`/`CSI 6 n` 及私有 `CSI ?6 n` 光标报告、设备属性 `CSI c` 响应、DECRQM `CSI ?Ps $p` 已跟踪模式查询、窗口/屏幕尺寸 `CSI 18 t`/`CSI 19 t` 响应，以及 `CSI J/K/@/P/X/b` 清除、插入、删除和重复字符与元数据同步清理；链接仅允许 `http://`、`https://`，宽字符 continuation 单元清除时会同步清理样式、组合字符和链接元数据；组合字符在退格/光标回退后的附着语义已覆盖，完整终端交互状态仍待实现。
- LSP manager、后台通知轮询、didChange/didSave/didClose 同步、5 秒请求超时与 `$/cancelRequest`、diagnostics 行号标记与当前行消息、completion 候选弹层、hover 信息弹层、definition/implementation/type definition/references 位置跳转与多位置选择弹层、workspace-edit Rename、整文件/选区 range formatting 和 Code Action 选择弹层已接入；Workspace Edit 的 `changes`/`documentChanges` 可写回其他文件，并支持 `CreateFile`/`RenameFile`/`DeleteFile` 资源操作；应用前会预检 URI、目标文件、只读状态、范围合法性、重叠编辑和资源操作覆盖策略，单文件多编辑合并为一次撤销；Rename 现在先展示按文件汇总的可视化预览，资源目标冲突会在预览中显式标注，Enter 应用、Esc 取消；LSP 服务状态已区分启动中、已连接和失败，服务进程异常退出或请求失败会标记失败并从活动客户端移除，后续同步可重新启动服务，状态栏会显示对应状态。
- Git 目前读取 branch/status/unified diff、保留显式/省略数量的 hunk header、commit 历史和 blame，并支持 Branches 侧栏创建/切换/删除、整文件/单 hunk/批量 stage/unstage、Changes 面板提交及 fetch/pull/push；Changes 面板可用 `d` 打开可滚动的 Output diff 视图，`v` 在 unified/split 视图间切换，`t` 打开独立 Git diff tab，标签内支持 `v` 切换 unified/split、`[`/`]` 选择 hunk，Enter 进入对应文件的可编辑 `HEAD` 基线 diff 模式；Output split 视图也可用 Enter 进入当前变更文件编辑，编辑后行号区实时标记新增/变更行；冲突文件会高亮并支持 Changes 面板 Ours/Theirs 快速检出后暂存，编辑器行号区以 `O`/`B`/`T`/`!` 标识冲突区域，编辑器内还可按标准 diff3 冲突块使用 `Ctrl-Alt-Up/Down` 循环定位、`Ctrl-Alt-O`/`Ctrl-Alt-E`/`Ctrl-Alt-B`/`Ctrl-Alt-K` 选择 Ours/Theirs/Base/Keep Both，`Ctrl-Alt-P` 打开当前冲突块的 Ours/Base/Theirs 三方预览并可直接选择，结果纳入单次 undo；普通保存、Save As 和退出批量保存会检测未解决冲突标记并阻止覆盖文件；远程操作认证失败后提供内存凭据输入、重试和取消，凭据仅通过进程环境传给一次性 Git credential helper，不写入日志、配置或工作区。
- Lua 插件目前能加载基础入口和安全系统 API，插件 keybinding 和 Ctrl-P 命令面板可实际触发已注册 command，`ttt.on` 可订阅 app/document/tick 以及 `tab.open`/`tab.activate`/`tab.close` 生命周期事件，sidebar/bottom panel 的 `render` 回调可通过 panel proxy 绘制常用文本/列表/表格，list/table 支持 selected/loading/empty、表格 cells 和右侧标记，以及 pager/tabs/scrollview/dialog/dropdown/password/checkbox/border 等常见 widget 的兼容降级内容；`require("ttt.editor")` 已按 `editor.read` 权限提供活动缓冲区全文、行数组、当前行、指定行、行数、光标、选区、选区文本、路径、文件名、语言和多光标快照，按 `editor.write` 权限提供插入、整行替换、范围替换、光标/选区和多光标操作，写入复用 Buffer undo checkpoint 并在插件回调后消费，Lua 坐标为 1-based rune 列；插件日志可进入 Output 面板，获得 `terminal.control`/`terminal.read` 权限的插件可通过 `ttt.terminal` 使用受管控的 portable-pty spawn/send/read/screen/resize/focus/close/list 终端 API，`screen` 返回 VT 解析后的可视文本，输出尾部限制为 1 MiB，file handler 已可实际打开为插件视图，tab proxy 提供 `id`/`title`/`focus`/`set_title`/`close`/`on_close` 入口且 `set_title` 会实际更新视图标题，`ttt.text.width/truncate` 已按终端列宽处理宽字符，`system.exec` 支持受权限保护的 stdin/env/timeout 选项，`exec_async` 已改为后台执行并在主循环回调；manifest 已支持 theme-only 插件、主题 descriptor 和包内路径校验，主题可在启动时加载，并可用 `Ctrl-Alt-T` 在内置/配置/插件主题间循环切换，应用到菜单、编辑器、侧栏、弹窗、Agent 面板、背景、面板、边框和状态栏；终端 ANSI/256 色仍按终端协议语义渲染。尚未覆盖 TTT 的完整交互 widget 状态和终端模块。
- AI Agent 已具备独立 PTY/VT 会话、独立输入路由、alternate screen 下字符/退格/回车/Tab/鼠标事件直通 PTY、bracketed paste 包装、兼容旧格式的 `agent.command`/`agent.cwd` 配置，以及 `agent.providers`/`agent.active` 多 provider 切换；新增 PATH 可用性检查、`install` 命令配置、显式确认后在普通 Terminal 中执行安装，以及安装完成后的后台可执行文件检测和 Agent PTY 自动重启；退出时会持久化 active provider、输入缓冲和屏幕文本快照，并在下次仍配置该 provider 时优先恢复 provider 与会话内容；Agent 及 provider 已支持显式 `tools` 权限清单，并默认拒绝未声明工具；新增受权限和 workspace 根目录限制的结构化 `read_file`/`list_files`/`write_file`/`search` 工具调用入口，写入使用临时文件替换且支持 workspace 内创建新文件，搜索不跟随符号链接并限制结果数量；Agent PTY 的 JSONL `tool_call` 消息现在由主循环异步执行并回写 `tool_result`，沿用同一权限和 workspace 校验。尚未提供真实 PTY 进程跨重启持久化。

补充：dialog checkbox 已具备状态保存、含 dropdown/button 混合布局的 Tab/Shift-Tab 焦点切换、Enter/Space 激活、内容行鼠标命中和 `on_change` 回调；dialog password 已接入受控输入、掩码渲染和回调；终端已解析 OSC 0/1/2 窗口/图标标题并显示在 Terminal 面板标题中，支持受 64 KiB 限制的 OSC52 剪贴板接收，并兼容 `ESC Z`、CSI 13/14/16/18/19 t 设备和窗口尺寸查询、DECPAM/DECPNM 应用键盘模式；PTY 退出通知具备幂等语义，只报告一次，主动 close 也具备幂等语义。

## 验证基线

插件兼容已覆盖 `ttt.editor` 读写与 undo group、`ttt.events` 多监听器和事件 payload（编辑器变更事件包含语言及选区状态/文本）、`ttt.diagnostics`、`ttt.bookmarks` 及对应权限；插件诊断与 LSP 诊断合并显示，书签图标显示在编辑器行号区。

插件 manifest 权限已兼容布尔值、字符串数组和路径数组；`ttt.system.env` 与 Host 主循环定时器 API 已接入并覆盖回调、清理和权限测试。

`ttt.automation.run` 已接入本地串行请求队列，支持 session/buffer、文本输入、按键、打开文件、截图/调试快照和完成回调，并已具备 RPC 连接入口。

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

当前基线：`cargo test --offline` 为 278 passed、1 ignored；Clippy 无警告，离线构建通过，`git diff --check` 通过。

## 下一阶段顺序

1. 完善 `ttt.editor` 与事件系统的更多 payload 和插件自动化桥接。
2. 完善终端组合字符状态和剩余 VT 交互状态覆盖。
3. 完善 Git 三方冲突编辑器和插件 diff 的更多编辑上下文。
4. 完善 LSP rename 冲突标注、更多 workspace edit 交互。
5. 完善 TTT widget 状态、插件 tab 生命周期和终端模块。
# 最近完成

- 插件事件 payload：`editor.change`/`cursor.change` 现在附带当前行文本、文档行数和 dirty 状态，并保持旧版回调参数兼容。
- 插件交互弹层：`ttt.confirm`、`ttt.show_info`，包含 overlay 焦点恢复、Enter/Esc 操作和确认回调。
- 插件命令行：`ttt.command_line.show/hide/set_text/active`，支持 prefix、初始文本、实时 `on_change`、提交/取消回调和可消费的 `on_key`，并接入 overlay 焦点恢复。
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
- 插件弹层鼠标交互：自定义 dialog 会按内容行定位 input、dropdown 和 button，鼠标点击可切换焦点并触发对应控件动作；键盘导航和关闭焦点恢复保持不变。
- 插件 Tab 生命周期清理：自定义 tab/file view 的 `on_close` 回调执行后删除 registry key，避免重复打开关闭累积 Lua 资源。
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
- Git 远程认证：fetch/pull/push 认证失败后提供内存凭据 overlay 和重试路径，凭据通过一次性 Git credential helper 传递，不写入磁盘。
- CLI 自动化：`--exec` 已接入启动阶段 JSON 动作执行，支持 `key`、`type`、`paste`、`wait` 和 `quit`，并对动作格式、按键和等待上限做校验；`--exec-split-on` 可拆分多个 JSON 脚本顺序执行。

