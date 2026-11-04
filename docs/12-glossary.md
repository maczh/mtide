# 术语表

| 术语 | 定义 |
|---|---|
| App | 组合 workspace、services、UI 和事件循环的应用状态 |
| Buffer | 可编辑的行文本及版本，不等同于文件 |
| Document | Buffer 加路径、保存、语言和外部变更状态 |
| Workspace root | 多根工作区中的一个目录 |
| Tab | 编辑器或 diff/特殊视图的标签页 |
| Position | buffer 坐标，line + character index |
| Visual column | 终端屏幕列，受宽字符、tab 影响 |
| EditCommand | 可执行、可撤销、可重做的编辑操作 |
| Fold | 不改变原文的可视范围折叠 |
| Diff overlay | 将 Git 差异映射到 editor 行的呈现层 |
| LSP | Language Server Protocol，代码理解协议 |
| PTY | pseudo-terminal，终端进程的字节通道 |
| Cell | 终端屏幕中的 glyph、颜色和属性单元 |
| Force key | terminal raw focus 时仍由应用优先处理的快捷键 |
| Generation | 异步请求或搜索结果的代数，用于丢弃过期结果 |
| Plugin permission | 插件访问文件、网络、进程等能力的授权 |
| Agent | 由 mtide 管理的持久 CLI 编程助手会话 |
