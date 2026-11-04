# 总体架构

## 1. 分层

```text
CLI / platform
      ↓
Application orchestration + command registry
      ↓
UI widgets / layout / focus / overlays
      ↓
Domain core: buffer, cursor, selection, undo, fold, diff, workspace
      ↓
Services: filesystem, watcher, git, LSP, PTY, plugin host, search
      ↓
OS / terminal / external processes
```

核心原则是“领域层不依赖终端 UI”。Rust 中 `core` 不得引用 crossterm/ratatui 等呈现库；终端事件由 adapter 转成应用事件。

## 2. 运行时

启动流程：

1. 解析 CLI 和环境变量。
2. 加载 settings、keybindings、theme、`.editorconfig`。
3. 创建终端或模拟屏幕，建立渲染缓冲。
4. 根据 targets 构造 Workspace/Document/Tab。
5. 注册命令、快捷键、插件和 LSP manager。
6. 进入事件循环：读输入 → 分发 → 执行命令/更新状态 → 处理异步消息 → layout/render → flush。

主线程拥有所有可见状态。后台任务只能发送带版本/请求 ID 的消息。

## 3. 事件模型

建议使用 `AppEvent`：

```text
Input(Key|Mouse|Paste)
Command(CommandId, Args)
Async(Lsp|Git|Search|Watcher|Pty|Plugin)
Timer(TimerId)
Resize(width, height)
Quit
```

每个异步请求带 `generation` 或 `request_id`。结果到达时，若文档版本、搜索代数或会话 ID 不匹配，直接丢弃。

## 4. 渲染管线

`AppState → layout → widget draw → current CellGrid → diff(previous,current) → terminal write`。

布局计算结果必须存储在对应 view state 中，鼠标事件复用同一坐标，避免渲染和命中测试偏移。显示宽度只能通过统一的 width service 计算；不能用 rune 数量代替终端列数。

## 5. 线程边界

| 组件 | 执行方式 | 约束 |
|---|---|---|
| UI、buffer、widget | 主线程 | 不共享可变引用 |
| ripgrep、Git、LSP | async task/worker | 结果带代数，回主线程 |
| PTY reader | dedicated task | 字节流转事件 |
| TextMate 增量高亮 | 主线程分步执行 | 避免一次性卡住 UI |
| 插件 Lua | 宿主控制上下文 | 权限检查和时间预算 |

## 6. 关键不变量

- `Cursor.col` 是字符/标量值索引，不是字节偏移，也不是终端列。
- 每次 buffer mutation 都产生一个可撤销 command。
- widget state 不在后台线程修改。
- terminal focus 时按键默认原样发送给 PTY，force key 除外。
- 不允许在 widget 中写死颜色；只引用语义 style。
- 保存、重命名、删除等外部副作用必须经过 service boundary。

## 7. UI布局截图

![image-20261104014457609](/home/macro/.config/Typora/typora-user-images/image-20261104014457609.png)
