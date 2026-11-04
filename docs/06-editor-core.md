# 编辑器内核

## 1. 文本表示

输入是 UTF-8。内部必须统一定义三种坐标：

| 坐标 | 用途 |
|---|---|
| byte offset | 文件、LSP UTF-8 转换、外部工具 |
| character/rune index | Buffer、Cursor、Selection |
| visual column | 屏幕布局、鼠标命中、宽字符渲染 |

禁止用 `len(string)`、字符数或 `col += 1` 推导屏幕列。East Asian wide、组合字符、tab 和控制字符都通过 `TextWidth` 服务处理。宽字符不能绘制在 clip region 最后一列。

## 2. 编辑命令

基础命令：insert、backspace、delete、newline、join/split line、move line、duplicate/delete line、toggle comment、sort lines、transform case、format selection。每个命令定义：输入范围、文本变更、光标结果、选区结果、undo/redo 行为和失败条件。

## 3. 查找替换

编辑器内搜索支持 smart-case、next/previous、replace-one/all；搜索面板使用独立 query/result generation。替换前必须固定匹配快照或从后向前应用，避免 offset 漂移。当前工作区搜索在查询无大写时对 ASCII 文本不区分大小写，查询含大写时区分大小写。

当前搜索侧栏按 `Ctrl-R` 切换 Replace 输入，Enter 对当前文档执行 Replace All，整个替换作为一个 undo batch；`Ctrl-Shift-Enter` 对当前搜索结果涉及的整个工作区执行替换，替换与搜索使用相同的 smart-case 规则，当前打开文档保留 undo/dirty 状态，其他文件直接落盘。

搜索焦点下 `Ctrl-N`/`Ctrl-P` 循环选择下一条/上一条工作区结果，结果跨多根 workspace 统一编号并在状态栏显示路径、行列；搜索结果列使用 UTF-8 字符列而非 byte offset，Unicode 文本跳转保持与编辑器光标一致。

## 4. 多标签

编辑器维护一个 `Vec<Buffer>` 与 active index。每个标签独立拥有光标、撤销/重做历史、保存基线和 dirty 状态；打开同一路径时复用已有标签，避免同一文件出现两个未同步副本。`Alt+Left/Right` 循环切换标签，`Ctrl-W` 关闭当前标签；dirty 标签必须先保存，最后一个标签不可关闭。

## 5. 多光标

`selectNext`、`selectAll`、Alt-click 创建 cursor。当前基础实现支持 `Ctrl-A` 全选、`Ctrl-Alt-N` 在当前单行选区后选择下一个相同文本、`Alt-Shift-I` 将跨行选区拆分为逐行 selections、`Alt-Up/Down` 在相邻行创建附加光标、编辑区 Alt-click 添加/移除附加光标；普通字符输入、粘贴、换行、退格和删除会对多选区生成批量 edit、逆序写入，并作为一个 undo batch。应用编辑前排序、去重、合并重叠 selection。

## 6. 折叠与导航

折叠由缩进范围产生，用户手动状态覆盖自动建议；Markdown 的 `<details>`/`</details>` 区块按成对标签识别为单独折叠区域。当前实现按 `Ctrl-Shift-[` 折叠光标所在的缩进块或 Markdown details 块，`Ctrl-Shift-]` 展开全部；折叠只改变视图，不修改 buffer，Up/Down 会跳过隐藏的折叠块。屏幕行到 buffer 行需要可逆映射；go-to-line、搜索、LSP 跳转必须穿透折叠并把目标滚入 viewport。

## 7. 高亮与性能

完整 TextMate grammar 规划为增量 document/token cache；当前渲染层提供轻量、无阻塞的扩展名语法着色，覆盖常见关键字、字符串、数字和注释。远处行延迟处理，主线程每次事件间执行有限 step。大文件可关闭 word-wrap、自动 reload 或 syntax highlight。打开文件会后台轮询磁盘 mtime：干净标签自动重载并重置旧 undo 基线，脏标签不覆盖内存内容而显示外部变更提示。

语言选择优先级为手动 override > 用户 grammar setting > 文件名/扩展名探测。Toggle Comment 从当前语言 grammar 取 comment token；没有 comment 规则时应提示不可用。

soft wrap 只改变 viewport 映射，不改变 buffer 行。Up/Down 在 wrap 开启时保持目标 visual row；wrap 关闭时支持 horizontal viewport、键盘滚动和鼠标滚轮。

当前编辑器会依据光标行自动调整垂直 viewport，保证光标在可见区域内；关闭 soft-wrap 时支持按 `Ctrl-Alt-Left/Right` 以 4 个字符为步长水平滚动，并在渲染时自动把光标保持在可见列内；启用 soft-wrap 时按配置列宽计算视觉行并同步光标/垂直滚动映射。启动时读取工作区 `.editorconfig` 的常用 `indent_style`、`indent_size`、`tab_width`、`insert_final_newline` 和 `trim_trailing_whitespace` 规则。

auto-closing 必须处理成对符号、跳过已有 closing rune、选区包裹和 undo 合并，并提供 opt-in 配置。

当前基础编辑器已支持 `()`、`[]`、`{}`、单引号和双引号自动闭合；输入已有 closing rune 时移动光标而不重复插入。

当前行命令：`Ctrl-Shift-D` 复制行，`Ctrl-Shift-K` 删除行，`Alt-Shift-Up/Down` 移动当前行或选中行，`Ctrl-J` 合并当前行与下一行或选中行，`Alt-Shift-S` 升序排序行，`Alt-Shift-U/L` 转换当前行或选中行大小写，`Ctrl-/` 根据文件类型切换 `//`、`#` 或 `--` 注释；这些行操作均为单个 undo batch。Enter 会继承当前行前导空白，并在 `{` 或 `:` 结尾后增加四个空格的块缩进。

选择与剪贴板：Shift+方向键/Home/End 创建选区；Ctrl-C 复制、Ctrl-X 剪切、Ctrl-V 粘贴。跨行选择以换行连接，选区删除和粘贴保持 UTF-8 字符列。复制或剪切时同时向宿主终端发送有 64 KiB 上限的 OSC52 序列，支持 SSH/无桌面环境的系统剪贴板；超出上限时仍保留内部剪贴板，但不发送外部序列。

冲突块：打开包含 `<<<<<<<`/`=======`/`>>>>>>>` 标记的文件后，光标位于冲突块时按 `Ctrl-Alt-O` 保留 Ours，按 `Ctrl-Alt-E` 保留 Theirs，按 `Ctrl-Alt-B` 保留 Base；`Ctrl-Alt-Up/Down` 在冲突块之间循环定位。替换为单次 undo，并保留未处理的其他冲突块。Changes 面板的 `o`/`t` 仍用于对整个冲突文件执行 Git checkout。
编辑器行号区会以 `O`、`B`、`T` 和 `!` 标识冲突内容、Base、Theirs 及冲突分隔线，便于在未解决状态下快速辨识三方区域。

鼠标选择：启用 Crossterm mouse capture 后，编辑区左键点击定位光标，左键拖动创建跨行选区，释放鼠标结束选择；按住 `Alt+Shift` 左键拖动创建矩形/列选区，列选区按每一行独立范围参与复制、剪切、替换和单次撤销；行号栏、水平视口和面板边界不参与文本列计算。

## 7. 格式化与保存

保存流程：检查 dirty → 可选 format-on-save/code actions → trim trailing whitespace → final newline → 原子写入 → 更新 mtime/version → 清除 dirty。格式化失败不得覆盖原文。`Ctrl-Shift-F` 无选区时请求 `textDocument/formatting`，存在选区时请求 `textDocument/rangeFormatting`，两者均使用 UTF-16 LSP 列。

终端程序启用 DEC bracketed paste（`CSI ?2004h`）后，编辑器通过系统粘贴或终端 `Ctrl-V` 发送标准 `ESC[200~`/`ESC[201~` 包装；未启用时保持普通输入行为。

终端处于 alternate screen 时，字符、退格、回车和 Tab 直接写入 PTY，保证 Vim、Ratatui、全屏 shell 等交互程序不会被编辑器的普通命令输入缓冲拦截。
同一模式下 Ctrl-A…Ctrl-Z 及 Ctrl-@、Ctrl-[、Ctrl-\\、Ctrl-]、Ctrl-^、Ctrl-_ 按 ASCII 控制字节直通 PTY。

多光标 clipboard：无选区时按 cursor 取整行；复制按文档顺序以换行连接；粘贴内容行数等于 cursor 数时按 cursor spread，否则每个 cursor 插入完整内容；多选区粘贴遵循同样的逐区段分发规则。Copy/Cut/Paste 每次都是一个 undo batch。

Find overlay 打开时保存原 focus；编辑器按 `Ctrl-F` 打开 Find 时，单行 selection 会初始化 query；Find 与 Replace 可以在同一 overlay 内切换；Ctrl+V、Copy、Cut 必须发送给当前 input 而不是 editor。
# LSP 快捷操作

编辑器焦点下：

- `Ctrl-Space`：请求 completion，并打开候选弹层。
- `Ctrl-H`：请求 hover，并打开信息弹层。
- `Ctrl-D`：请求 definition，并打开目标文件定位到目标位置。
- `Ctrl-Alt-R`：请求 references，并打开首个引用位置。
- `Ctrl-Alt-I`：请求 implementation，并打开首个实现位置。
- `Ctrl-Alt-T`：请求 type definition，并打开首个类型定义位置。

completion 弹层使用 Up/Down 选择、Enter 插入、Esc 关闭。

completion、hover 和 code action 均通过可交互弹层呈现；completion/code action 支持 Up/Down 选择、Enter 应用和 Esc 取消。

