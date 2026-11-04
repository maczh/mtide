# 插件与 AI Agent

插件 tree/list 的 `on_select(node)`、table 的 `on_select(row_index)`、table 的 `on_activate(row_index)` 和 tabs 的 `on_change(tab_id)` 回调会在插件标签中随上下翻页选择移动、按 Enter 或在侧栏/底栏面板中用鼠标点击激活；table 的 `key_commands` 会把单字符快捷键映射到 `on_command(command, row_index)`，插件标签 Enter 优先调用 table 的 `on_activate`，没有时回退到 `on_select`，Host 同时保留 tabs 的 `on_select` 兼容别名。tree 支持 `children`/`expanded` 递归渲染，tree/list/table 行支持 `icon`、`badge`、`right` 和 `actions` 文本兼容。回调执行后 Host 会刷新标签内容并消费编辑请求。

插件面板和自定义标签支持 progress widget：传入 value（0 到 1）、可选 label 和 char，Host 会按终端文本宽度渲染进度条并将越界值安全限制在有效范围。

Find 面板支持 Ctrl-Alt-R 在 smart-case 与正则模式之间切换，当前模式会显示在搜索面板中。

编辑器右键上下文菜单：声明 editor.read 权限后，插件可调用
ttt.editor.register_context_menu(callback)。Host 传入 1-based line、col 和当前单词
word，回调返回带 label/on_select 的菜单项数组，也支持 separator=true 分隔项。
用户可用鼠标或上下键选择、Enter 执行、Esc 取消；菜单回调结束后才消费编辑请求。

## 1. 插件生命周期

`discover → read manifest → validate → permission prompt → load → init → wire → active → reload/disable/uninstall`。

Lua 模块通过 `require("ttt")` 注入，不假设全局变量。插件加载阶段不能调用依赖 `WirePlugin` 后才可用的 API；命令或事件回调中才能调用通知、状态栏、执行命令等函数。

## 2. 插件能力

`tabs` 支持原版 `active=id`，并兼容数字 `selected`；状态在重绘间保持，切换时调用 `on_change(tab_id)`。`radio({options, selected, on_change})` 提供单选列表，`selected` 兼容 1-based 数字或选项字符串/id/value/label，方向键切换并将选中选项传给回调。`scrollview` 已按集合控件渲染 `selected`、`loading`、`empty` 状态；`pager` 支持原版 `offset/limit/total` 并兼容 `page/pages`，保存当前页状态，Left/Right、上下、PageUp/PageDown、Home/End 移动会调用 `on_change(new_offset)`，文本显示当前范围与总数。

`require("ttt.editor")` 已提供受 `editor.read` 权限保护的活动缓冲区只读接口：`buffer_text()`、`buffer_lines()`、`current_line()`、`get_line(n)`、`line_count()`、`cursor()`、`selection()`、`selection_text()`、`file_path()`、`file_name()`、`language()` 和 `get_cursors()`。`viewport()` 返回 1-based 的 `top_line`/`bottom_line`、当前可见行数 `height` 和水平 `left_col`；UI 绘制后会同步真实编辑区高度。Lua 行列统一采用 1-based，列按 Unicode rune 计数；没有活动选区时 `selection().active` 为 `false`。

声明 `editor.write` 后，插件可调用 `insert(line, col, text)`、`set_line(line, text)`、`replace(start_line, start_col, end_line, end_col, text)`、`set_cursor(line, col)`、`set_selection(...)`、`clear_selection()`、`add_cursor(line, col)`、`clear_cursors()` 和 `go_to_diff_hunk(index)`；`editor.diff()` 在活动可编辑 Diff 中返回 `hunks`（`file_path`、`kind`、1-based `line`）及 `current_hunk`，普通文件中返回 `nil`。`begin_undo_group()` / `end_undo_group()` 可将连续写入合并成一次撤销操作，并支持嵌套。写入调用进入 Host 编辑请求队列，在插件回调结束后应用，并复用 Buffer 的 undo checkpoint；非法或越界坐标会被安全限制到当前缓冲区范围。

`require("ttt.events")` 提供与 `ttt.on(event, callback)` 等价的 `events.on(event, callback)` 注册入口。同一事件允许多个插件回调同时订阅，Host 按注册顺序依次触发；单个回调失败会被记录，但不会阻断同一事件的后续回调，派发结束后仍向宿主报告首个插件错误。文件/编辑器事件会附带活动路径及必要的 1-based 行列参数：打开/重载/保存事件传入 `path`，`editor.change`/`cursor.change` 传入 `path, line, col, language, selection_active, selection_text, current_line_text, line_count, dirty`；旧版忽略额外参数的无参回调保持兼容。

`require("ttt.diagnostics")` 提供 `publish(path, items)` 和 `clear([path])`，需要 `editor.diagnostics` 权限。每个 item 使用 1-based `line/col`，可选 `end_line/end_col`、`severity`（`error`/`warning`/`info`/`hint`）、`message` 和 `source`；重复发布会替换该插件在该文件上的诊断。Host 将插件诊断与 LSP 诊断合并显示，插件禁用或重载时其运行时状态自然清空。

`require("ttt.bookmarks")` 需要 `editor.bookmarks` 权限，所有操作针对当前活动文件：`set(line, icon, style)`、`get(line)`、`remove(line)`、`set_all(items)`、`get_all()` 和 `clear()`。书签图标显示在编辑器行号区，书签数据只存在当前运行时；插件可自行持久化。设置、移除、批量设置和清空会排队触发 `bookmark.change(path, line, action, icon, style)`；用户点击行号左侧书签列时会触发 `gutter.click(path, line)`，插件可据此实现书签切换。两个事件均受 `editor.bookmarks` 权限保护；没有监听器时点击保持编辑器原有行为。

`require("ttt.automation")` 提供 `run(action, args, callback)`。Host 在主循环串行执行 `session`、`buffer`、`type`、`paste`、`key`、`open`、`screenshot` 和 `debug` 动作，完成后调用 `callback(result, error)`；返回的取消函数可取消尚未执行的请求。`key` 支持常见组合键和空格分隔的连续按键，`open` 参数为 JSON 对象字符串。该模块是单进程 Host 自动化桥接，跨进程会话服务仍由后续 `ttt.rpc` 适配层提供。

`require("ttt.rpc")` 支持 `connect(endpoint, options, callback)`，接受 `unix:` 和 TCP endpoint，并返回具有 `notify(method, params)`、异步 `request(method, params, callback)`、`close()` 的连接对象；需要 `network.rpc` 权限，出站使用 TTT/LSP 兼容的 JSON-RPC `Content-Length` framing，入站同时兼容旧版换行 JSON。manifest 中 `network.rpc=true` 允许任意 endpoint；字符串数组仅允许精确声明的 TCP/Unix endpoint，空数组拒绝全部连接。`request` 返回可调用的取消函数，取消会删除本地 pending 回调并发送 `$/cancelRequest`，重复取消或响应已到达时返回 `false`。`options.on_notification(method, params)`、`options.on_request(method, params)` 和 `options.on_close()` 分别接收服务端通知、反向请求和断开事件；反向请求回调的返回值会作为 JSON-RPC result，回调异常会转换为错误响应。网络读取在线程中完成，Lua 回调统一在 Host 主循环执行，避免跨线程访问 Lua 状态。

`require("ttt.decorations")` 在 `editor.decorations` 权限下提供 `set(path, ranges)` 与 `clear(path?)`。范围使用 1-based `line`/`end_line`/`col`/`end_col`，可附带 `style` 和 `note`；当前 Host 将其接入诊断/行号标记链，因此插件装载和 MCP 的装饰提示可直接工作。

`ttt.editor` 还提供 `byte_to_col(text, byte_offset)`、`col_to_byte(text, col)`、`viewport()`、`scroll_to(line)` 和 `scroll_by(delta)`；列为 1-based Unicode rune 列，字节偏移只用于 UTF-8 边界转换。滚动请求由 Host 主线程应用，默认渲染仍按光标自动保持可见。

`ttt.editor.set_search(pattern, use_regex?)` 和 `clear_search()` 可由插件驱动 Find 状态并刷新工作区结果；`use_regex=true` 使用 Rust regex 引擎，非法表达式返回空结果，普通模式保持 smart-case 语义。

在相应权限下，`ttt.key(combo)`、`ttt.type(text)`、`ttt.click(x, y)`、`ttt.drag(x1, y1, x2, y2)` 和 `ttt.quit()` 会转换为 Host 自动化队列动作：输入要求 `input` 权限，退出要求 `commands` 权限；动作在主循环执行，避免插件直接修改 TUI 状态。自动化队列还支持 `click`（`x y`）和 `drag`（`x1 y1 x2 y2`），事件使用当前 Ratatui/Crossterm 窗口区域分发。

在 `screen.read` 权限下，`ttt.screenshot(path?)` 返回当前编辑器快照文本；`ttt.debug(path?)` 还要求 `editor.read` 与 `terminal.read`，并返回包含路径、语言、光标、选区和缓冲区行数的 JSON 调试快照。带路径参数时两者都需要 `fs.write` 覆盖目标路径，Host 会按 manifest scope 写入插件目录/授权目录并返回 `true`。

插件文件视图的 `tab.render` 使用同一组 `label`、`list`、`table`、`tabs`、`scrollview` 和 `pager` 控件语义，避免 Sidebar 与自定义文件视图出现 API 分叉。

`sidebar` 与 `bottom` 注册面板按各自位置渲染；Bottom panel 不再被错误地显示到 Sidebar。

可注册 Sidebar/Bottom/Drawer/Editor panel、command、keybinding、file handler、status item、tab 和 Widget。Widget 包括 tree/table/list/input/dialog/dropdown/tabs/scrollview/markdown/image，并支持命名 style 和必要的 raw cell API。

`panel:image({src, alt})` 在当前无 Kitty/iTerm 图形协议时使用文本降级（优先显示 `alt`，同时保留 `src`），因此图像型插件仍可正常加载和表达内容；`ttt.graphics()` 会报告当前协议能力。

兼容的近期插件 API 还包括 pager、border/title/password input、checkbox（`checked` 与 `label/text`）、table row actions/selected/loading/empty/inset、tree/list right badge、hstack/vstack/box/scrollview 容器 render、keyvalue、divider、`panel:size()` 当前 Sidebar/Bottom/Drawer/自定义 Tab 内容区尺寸、带坐标的顶层 raw text/cell/clear（支持多行文本、空白填充和区域清理）、hstack align、表格水平滚动（`scroll_x`/`horizontal_scroll` 与可选 `width`，按 Unicode 字符裁剪）、双击选择复制、插件 selectionStyle、插件 tab id/on_close/focus、读取/导航 diff tabs、bundled theme manifest、`ttt.text` terminal-column width、`ttt.terminal` 的 spawn/send/read/screen/focus/close/list 进程终端 API、`sys.spawn` 交互 stdio、`sys.exec` 的 env/timeout/cancel 和 `sys.exec_async` 限长流式输出。

权限按文件、网络、process、terminal、storage、screen 和事件订阅拆分；`events.file` 保护 `file.open`/`file.close`/`file.save`，`events.editor` 保护 `editor.change`/`cursor.change`/`tab.change`，`editor.bookmarks` 保护 `bookmark.change`/`gutter.click`，`keybindings` 保护 `key.press`。权限拒绝必须返回可处理错误；插件整体禁用时核心编辑器仍可运行。

## 3. Manifest 与升级

插件 `status_items`/`statusItems` 注册项会显示在 Host 底部状态栏，并随插件运行时一起聚合。运行时还兼容原版 TTT 的 `ttt.set_status_item(side, id, text, opts)` 和 `ttt.remove_status_item(id)`：`side` 仅接受 `left`/`right`，`opts.priority` 决定同侧显示顺序，`opts.on_click` 可由鼠标点击触发；调用方需要 `commands` 权限。重复 id 会更新原项，移除插件或重载时由 Host 清理插件状态项及回调。

插件可通过 `ttt.exec_command(id)` 执行自己已注册的命令，并用 `ttt.list_commands()` 获取当前插件命令的 `{id, title}` 列表；两个接口都受 `commands` 权限保护，未知命令返回 `false`。

`ttt.show_panel("sidebar"|"bottom")` 和 `ttt.focus_panel("sidebar"|"bottom")` 会把已注册的插件面板重新显示到对应区域；后者还会把 Host 焦点切换到该区域。调用前必须声明对应的 `panel.sidebar` 或 `panel.bottom` 权限，且插件必须已经注册该位置的面板。

`ttt.open_drawer(config)` / `ttt.close_drawer()` 已兼容原版 Drawer schema：`config` 支持 `side`（`left`/`right`）、`width`、`min_width` 和必需的 `render(panel)`，需要 `panel.drawer` 权限；Drawer 由 Host 作为编辑器覆盖层渲染，重复打开会替换当前插件 Drawer。Drawer 中的 `button({on_click})` 已接入鼠标点击和 Lua 回调执行，`input({text, placeholder, on_change, on_submit})` 支持鼠标聚焦、字符输入、退格和 Enter 提交，`checkbox({checked, on_change})` 支持状态保持、鼠标聚焦和 Enter/Space 切换，`dropdown({options, selected, on_change})` 支持鼠标聚焦、方向键循环和 Enter 触发回调，容器 `render`、`keyvalue`、`divider` 与顶层 raw `text/cell` 可用于构建 `ttt-mcp` 的 JSON Drawer。

`ttt.open_tab(config)` / `ttt.focus_tab(id)` / `ttt.close_tab(id)` 已支持自定义编辑器标签：`config` 需要 `title` 与 `render(panel)`，可选 `id`/`focus`/`on_close`/`on_event`，并要求 `panel.editor` 权限；`on_event(event)` 可消费未被宿主导航快捷键占用的按键事件，也可接收编辑器内容区的鼠标事件。按键事件包含 `type="key"`、`key`、可选 `rune` 与 `mod` 字段；鼠标事件包含 `type="mouse"`、零基 `x`/`y` 和 `button`（`left`/`right`/`middle`/`wheel_up` 等）。回调返回 `true` 即消费事件，返回 `false` 时继续执行宿主的滚动、选择等行为；Host 会保留自定义标签的 ID 和标题、在每次主循环刷新其渲染内容，并支持按 ID 聚焦或关闭。重复打开同一 ID 会复用该标签标识并更新其内容；未知 ID 的 `focus_tab`/`close_tab` 返回 `false`。

`editor.file_handlers` 的 `open(path, tab)` 也支持在 `tab` 上设置 `on_event(event)`；文件视图获得焦点后，除宿主滚动、关闭、差异导航等保留快捷键外的按键和内容区鼠标事件会交给该回调，鼠标事件使用与自定义 Tab 相同的 `type`、相对 `x/y` 和 `button` 字段，返回 `true` 即消费事件，关闭视图时会同时清理 `on_close` 与 `on_event` 注册资源。处理器可用 `extensions` 按扩展名匹配，也可用 `magic` 声明最多 512 字节的文件头；文件头匹配优先于扩展名，且无扩展名文件也可被识别。`tab:focus()` 会请求 Host 聚焦当前插件文件视图，`tab:close()` 会请求 Host 关闭当前插件文件视图；渲染回调可保存传入的 `panel` 并在异步任务完成后调用 `panel:redraw()`，Host 会重新执行该文件视图渲染。

`ttt.open_readonly(title, lines, file_path)` 可直接创建只读内容标签，自动将字符串数组渲染为面板行；该接口不要求额外权限，返回标签 ID，Host 会保留可选 `file_path` 作为标签资源上下文。

`ttt.open_diff(title, old_lines, new_lines, file_path)` 可创建只读差异标签，按行级 LCS 生成上下文（`  `）、删除（`- `）和新增（`+ `）行，并复用自定义编辑器标签生命周期；超大输入采用有界的删除/新增回退策略；Host 会保留可选 `file_path` 作为差异资源上下文。

`ttt.open_file(path, line, readonly)` 会向 Host 请求打开工作区文件，支持 1-based 行号定位和只读打开；相对路径按当前工作区根解析，读取失败会转为 Host 状态栏错误。只读缓冲区拒绝编辑、撤销/重做和保存操作。

`ttt.clipboard_write(text)` 要求 `commands` 权限，会更新 Host 内部剪贴板并通过 OSC52 通道向外部终端同步；`ttt.clipboard_read()` 需要单独的 `clipboard.read` 权限，并读取 Host 当前同步的剪贴板内容。因此在 SSH 或容器环境中也能完成插件复制与受控读取操作。

manifest 至少包含名称、版本、入口、兼容版本和权限。安装来源可为本地文件、URL、社区 registry；下载后校验路径与 manifest，更新时保留旧版本回滚信息。

启动时插件自动发现遵循以下顺序：显式 `--plugin PATH` 参数优先；随后扫描全局 `~/.config/ttt/plugins/`（可由 `XDG_CONFIG_HOME`、`MTIDE_CONFIG_DIR` 或 `TTT_CONFIG_DIR` 定位）和工作区 `<workspace>/plugins/`。目录按名称稳定排序，按 manifest 的 `name` 去重，已由显式参数或全局目录提供的同名插件不会被工作区副本覆盖；manifest 无效或目录不可读时跳过该包，不阻断宿主启动。主题插件也走同一发现流程。

权限值兼容布尔值、字符串数组和路径数组；文件权限数组会作为实际路径 scope 校验，`{workspace}`、`{config}`、`~`、绝对路径和 `*` 分别表示工作区、配置目录、用户目录、指定目录和全局范围，插件包目录始终可访问；命令权限数组会作为 `system.exec`/`spawn` 的可执行文件 allowlist。`ttt.system.env(name)` 需要 `system.env` 权限；`ttt.set_timeout(ms, callback)`、`ttt.set_interval(ms, callback)`、`ttt.clear_timeout(id)` 和 `ttt.clear_interval(id)` 由 Host 主循环调度，回调不会在后台线程直接执行。

插件文件权限需限制在 manifest 声明路径；权限撤销、插件禁用、卸载和 reload 必须清理 decorations、status items、tabs、timers 和 pending callbacks。HTTP 每次 redirect 都重新校验目标权限；用户批准的自定义 binary path 也必须显式授权。

当前 Rust host 已实现 Lua 5.4 嵌入、`require("ttt")`、`ttt.log`、`ttt.notify`、`ttt.register`、`ttt.on`、`ttt.version/platform/arch/plugin_dir`，以及 `require("ttt.json")` 的 encode/decode。注册的 command 回调保存在 Lua registry 中，插件 keybinding 和 `Ctrl-P` 命令面板可触发对应 command；sidebar/bottom panel 支持 `render` 回调返回文本并绘制，panel proxy 的 `list/table` 已处理 `items/rows`、1-based `selected`、`loading`、`empty`、table 行 `cells` 和右侧 `right` 标记，并对 pager、tabs、scrollview、dialog、dropdown、password、border、badge、hstack/vstack、row_action 等常见 TTT widget 提供兼容入口；`app.start`、`app.tick`、`document.open`、`document.save` 会触发订阅回调；`require("ttt.events")` 的文件/编辑器/按键事件订阅已按 manifest 权限拒绝未授权注册；`require("ttt.terminal")` 在 manifest 授权 `terminal.control`/`terminal.read` 后提供受管控的 portable-pty `spawn/send/read/screen/resize/focus/close/list` 终端 API，`screen` 返回 VT 解析后的可视文本，`list` 返回尺寸，输出尾部限制为 1 MiB，`focus(id)` 会实际显示并聚焦 Host Terminal 面板；支持扩展名和最多 512 字节 `magic` 文件头匹配且 magic 优先的 `file_handlers.open(path, tab)` 可生成插件文件视图，tab proxy 提供 `id`、`title`、`focus`、`set_title`、`close` 和 `on_close` 生命周期入口，其中 `set_title` 会实际更新插件文件视图标题，`tab.on_close` 会在 Host 替换/关闭插件视图时回调并清理 Lua registry 资源；`ttt.open_tab({id,title,render,on_close})` 的自定义标签也会在 Host 关闭、替换或插件主动关闭时执行 `on_close` 并清理注册回调；另外 `tab.open`、`tab.activate`、`tab.close` 会覆盖普通编辑标签和插件视图的打开、切换、关闭生命周期。manifest 支持 `themes` 字符串或 descriptor 声明，主题资源型（无 Lua entry）插件可加载，且所有主题路径都会做包目录越界校验；首个有效主题可在启动时加载并应用到菜单、编辑器、侧栏、弹窗、Agent 面板、背景、面板、边框和状态栏。`require("ttt.system").exec` 仅在 manifest 声明 `system.exec` 权限时可用，支持 `options.stdin`/`options.env`/`options.timeout_ms`，超时会终止子进程并返回 `timed_out`；`exec_async` 在后台线程执行并由主循环回调 Lua；`system.spawn(options)` 同样要求 `system.exec`，返回带 `write`、`close_stdin`、`kill`、`pid` 的长生命周期进程句柄，并通过 `on_stdout`、`on_stderr`、`on_exit` 回调把流式输出和退出状态交回 Host 主循环；`require("ttt.fs")` 提供受 `fs.read`/`fs.write` 保护、限制在插件目录内的 read/write；`ttt.text.width/truncate` 按终端列宽处理宽字符；拒绝权限时返回 Lua 错误。LSP 重命名请求会先进入按文件汇总的预览弹层，用户确认后才应用 Workspace Edit。
`require("ttt.storage")` 在 manifest 声明 `storage` 权限后提供 `get(key)`、`set(key, value)`、`remove(key)`、`keys()`；值兼容 JSON 可表示的字符串、数字、布尔值和嵌套表，数据按插件名称私有保存到配置目录的 `plugin-storage/<name>.json`，单插件上限 1 MiB，`set(key, nil)` 等价于删除，未授权插件得到空模块且不会暴露存储函数。

沙箱内置全局 `crypto` 表（同时兼容 `require("ttt.crypto")`）提供 `base64_encode`、支持外围空白与省略 padding 且返回 `nil,error` 的 `base64_decode`、系统 CSPRNG 生成的十六进制 `random_bytes` 和 RFC 4122 v4 `uuid`；随机长度限制为 1–1024 字节，避免插件无界申请资源。

异步命令 API：`system.exec_async(...)` 返回带 `cancel()`/`id()` 方法的句柄，在后台执行并由主循环回调；`system.cancel_async(id_or_handle)` 兼容显式取消入口，回调结果包含 `cancelled`。`ttt.fs` 的读写、列举和存在性检查均遵循 manifest 声明的路径 scope；插件包目录始终可访问，越界路径返回可处理的错误。

`system.exec` 与 `system.exec_async` 支持 `options.max_output_bytes`；stdout/stderr 共享上限，超过后终止子进程，仅保留上限内的首段输出，并返回 `truncated=true`、`exit_code=-1`、`ok=false`。`timeout_ms`、取消和输出超限均不会把被终止进程误报为成功。

`exec_async` 的 options 支持 `on_stdout(chunk)` 与 `on_stderr(chunk)`；Host 在线程读取过程中把 UTF-8 降级后的输出块投递到主线程 Lua 回调，最终结果回调仍只执行一次。取消、超时或输出截断后不再向插件投递新的输出块。

`system.spawn(binary, options)` 现在兼容 TTT 标准签名，`options` 支持 `args`、`cwd`、`env`、`on_stdout`、`on_stderr` 和 `on_exit`；原有 `system.spawn({shell, args, ...})` 形式继续可用。返回句柄提供 `write`、`close_stdin`、`kill` 和 `pid`。
`require("ttt.regex")` 提供无权限要求的 RE2 风格正则：`compile(pattern, {ignore_case = true})` 返回正则对象或 `nil, error`，对象支持 `find(text, init)` 和 `find_all(text, limit)`，结果使用 1-based 字节区间并返回捕获组表。
`require("ttt.settings")` 在 manifest 声明 `settings` 且通过 `permissions.settings_keys` 指定键范围后提供 `get(key)`/`set(key, value)`；键使用点号路径，支持精确键和 `group.*` 通配范围，配置文件不存在时读取与 Host 相同的默认设置，修改会立即持久化到配置目录的 `settings.json`，写入 `nil` 删除键；启动参数 `--config FILE` 时插件与 Host 共用该配置文件。

## 4. AI Agent
`Ctrl-P` 命令面板同时包含内置编辑器、视图、工作区命令和插件命令，支持按命令 ID 或标题过滤并直接执行。

Agent 是右侧持久终端会话，不与普通 bottom terminal 共用状态。Provider 配置包含 executable、args、cwd、可选 install command、显示名和启用状态。Host 同时兼容 mtide 的 `agent.providers` 和 TTT 的 `agents` 顶层 schema（`cli`、字符串/数组 `args`、`install`），统一归一化后再启动。Host 会先检查 executable 是否存在于 PATH；缺失时在右侧显示完整安装命令，只有用户按 Enter 明确确认后才会把命令发送到普通 Terminal 执行，禁止启动阶段静默安装。安装命令执行期间保持 pending 状态；一旦 executable 进入 PATH，Host 自动重建该 provider 的独立 Agent PTY 会话，切换 provider 会取消旧 pending 状态。

Agent 默认输出是 PTY 文本，不直接改变编辑器状态；Agent 进入 alternate screen 后，字符、退格、回车和 Tab 直接写入其 PTY，bracketed paste 和启用的鼠标协议也使用标准终端事件。Agent 也可逐行发送 `{"type":"tool_call","id":...,"tool":"read_file|list_files|write_file|search","args":{...}}`，Host 在主循环异步执行并回写 `tool_result` JSONL，权限和 workspace 根目录校验与本地工具入口一致。Host 会在退出时保存 active provider、未提交输入和 VT 屏幕文本快照，下一次启动且 provider 一致时恢复这些可视状态，PTY 进程本身不会被伪持久化。

## 5. 安全边界

插件/Agent 能够执行外部命令，应在 UI 中明确显示来源、权限和 cwd。日志与 debug dump 脱敏；网络权限默认关闭或按插件声明询问。

Lua 沙箱仅保留 TTT 约定的安全运行时：`os` 只暴露 `time`、`clock`、`date`，`io`、`debug`、动态加载函数和 `print` 等入口被移除；`package` 只允许从内置 `preload` 加载受支持的 `ttt.*` 模块，文件系统、网络和进程能力仍必须经过 manifest 权限校验。
# 兼容性补充：交互弹层

插件可调用 `ttt.confirm(message, callback)` 打开确认弹层。用户按 Enter 时执行回调，按 Esc 取消；确认期间焦点被 overlay 接管，关闭后恢复原焦点。

插件可调用 `ttt.show_info(title, entries)` 打开只读信息弹层，其中 `entries` 为 `{key = "...", value = "..."}` 数组。按 Enter 或 Esc 关闭。

插件可调用 `ttt.open_dialog({title, width, height, render, on_close})` 打开通用插件弹层，内容复用 panel proxy 渲染；`ttt.close_dialog()` 可主动关闭。弹层中的 `panel:input({text, placeholder, on_change, on_submit})` 和 `panel:password({text, value, on_change})` 已支持字符输入、退格、实时回调和 Enter 提交，密码内容按掩码渲染；`panel:checkbox({checked, label/text, on_change})` 支持状态保持、Tab/Shift-Tab 焦点循环和 Enter/Space 切换，多个 `panel:button({label, on_click})` 支持 Tab/Shift-Tab 双向焦点循环、Enter 和按内容行命中的鼠标点击；dropdown 也支持按行命中后用方向键/Enter 操作。Esc 和主动关闭都会先恢复原焦点，再执行 `on_close`。

插件可通过 `require("ttt.command_line")` 使用单行命令输入：`show({prefix, text, on_change, on_submit, on_cancel, on_key})`、`hide()`、`set_text(text)` 和 `active()`。该模块要求 `keybindings` 权限；输入期间所有按键由命令行优先处理，提交前先关闭 overlay，再执行 `on_submit`，取消执行 `on_cancel`，焦点随后恢复；`on_key` 出错时记录错误并按未消费处理。

`sidebar`/`bottom` 注册面板可声明 `on_event(event)`；当对应面板获得宿主焦点时，按键按 `type="key"`、`key`、可选 `rune`/`mod` 传入回调，返回 `true` 即消费事件，返回 `false` 则继续执行宿主默认导航。

所有 panel proxy（包括 Sidebar、Bottom、Drawer 和自定义标签）都提供 `panel:redraw()`；插件在异步任务更新自身状态后调用它，会向 Host 请求下一轮该面板重绘。面板、标签页和文件视图的 `on_event` 回调发生 Lua 错误时，Host 会记录错误并按未消费处理，保证故障插件不会卡住宿主输入。

`require("ttt.events")` 可在声明 `keybindings` 权限后订阅 `key.press`。该事件只在编辑器获得焦点且没有 overlay、终端或 Agent 等更高优先级输入模式时触发；事件字段为 `type="key"`、`key`、可选 `rune`/`mod`，回调返回 `true` 会消费按键，返回其他值则继续执行编辑器默认行为。插件命令键绑定和宿主强制快捷键优先于该事件；回调发生 Lua 错误时 Host 记录错误并按未消费处理，确保输入不会被故障插件卡住。

面板中的 tree/list/table/tabs/dropdown 也支持宿主键盘选择：Up/Down/PageUp/PageDown/Home/End 更新选中项并调用 `on_select`/`on_change`，Enter 对 table 优先调用 `on_activate`，没有时调用 `on_select`；树节点存在 `children` 时 Enter 会切换 `expanded` 并调用 `on_expand(node)`，叶节点仍调用 `on_select(node)`；tree/list 的 `key_commands` 会将命令和当前节点传递给 `on_command(command, node)`，table 保持 `on_command(command, row_index)`；普通面板 dropdown 的方向键会改变选项，Enter 调用 `on_change(value)` 或 `on_menu(command)`；宿主会在重绘时保留选中行。

`ttt.debounce(milliseconds, callback)` 返回 `run(...)` 与 `cancel()` 两个函数。连续调用 `run` 会取消旧计时并只把最后一次参数传给 callback；计时器在 Host 主循环执行，`cancel()` 可取消尚未触发的回调。

`ttt.on_install(callback)` 可注册一次安装钩子。插件正常启动加载时不会执行；通过社区 Registry 安装并成功加载后由 Host 执行一次，回调异常会使本次安装失败并回滚安装目录。

`ttt.on_uninstall(callback)` 可注册一次卸载清理钩子。Host 的卸载流程显式调用 `run_uninstall_hook`，回调异常会保留失败结果供上层决定是否重试或强制移除；正常加载不会执行该钩子。

`ttt.markdown(text)` 返回 1-based 行数组，每行由 `{text, style}` span 组成；当前解析支持普通文本、`#`/`##`/`###` 标题、`**粗体**`、`*斜体*`/`_斜体_` 和 `` `行内代码` ``，便于插件将 Markdown 转成 panel 内容。

`ttt.executable_path()` 返回当前 mtide 可执行文件的绝对路径，供插件调用自身宿主或展示诊断信息使用。

`ttt.open_changes({id, title, description, diff, file_path})` 可将包含 `diff --git` 文件头的 unified diff 打开为插件只读差异标签；Host 保留描述和原始 diff 行并复用自定义 tab 生命周期，插件标签获得焦点后支持 Up/Down/PageUp/PageDown/Home/End 滚动、`[`/`]` 在 `@@` hunk 间跳转、`v` 切换 unified/split 视图、Esc 或 Ctrl-W 关闭。提供 `file_path` 时，Enter 会打开对应工作区文件并进入可编辑基线 diff 模式。

插件可通过 `require("ttt.net")` 使用受 `network.http` 权限保护的 `get`、`post`、`get_async` 和 `post_async`。同步/异步请求都限制为 HTTP(S)、30 秒超时和 10 MiB 响应体，并返回 `{status, body, headers, error?}`；异步网络线程只投递完成事件，Lua 回调在主循环执行。`network.http=true` 允许所有目标；声明字符串数组时按主机名/IP 和可选端口精确匹配，未列出的目标在请求离开进程前即被拒绝，自动重定向也会逐跳执行同样的目标校验。

`require("ttt.fs")` 在 `fs.read` 权限下提供 `read(path)`、`exists(path)` 和 `list(directory)`，在 `fs.write` 权限下提供 `write(path, content)`；插件包目录始终可用，manifest 中的 `true` 会额外允许当前 workspace，字符串数组可声明 `{workspace}`、绝对路径、`~/...`、`{config}/...` 或 `*` 范围。所有实际路径都会在 I/O 前做 canonical 边界校验，`list` 返回按名称排序的 `{name, is_dir}` 数组。文件不存在、目录读取失败等普通 I/O 错误返回 `nil, error_string`；权限拒绝和路径越界仍直接返回 Lua 错误。

`network.http: []` 明确拒绝所有网络目标；只有布尔值 `true` 才表示全目标放行，字符串数组则按声明的主机/IP 与端口精确匹配，重定向继续使用同一策略逐跳校验。

`require("ttt.system")` 的 `exec`、`exec_async` 与 `spawn` 在 `system.exec=true` 时允许全部命令；当 manifest 使用字符串数组时，只允许数组中声明的可执行文件，未声明命令会在启动子进程前拒绝。

