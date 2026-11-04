# 产品与功能需求

## 1. 产品定义

mtide 是键盘优先、单二进制、运行在终端中的现代代码编辑器。目标是在 SSH、容器、低带宽和无桌面环境下，提供接近 IDE 的编辑、代码智能、Git、终端、插件和 AI Agent 工作流。

目标用户：终端开发者、远程开发者、Git/LSP 用户，以及需要 Lua 扩展能力的高级用户。

## 2. 功能范围与优先级

| 优先级 | 范围 | 验收结果 |
|---|---|---|
| P0 | 启动、单/多根工作区、文件打开、文本编辑、撤销、选择、导航、Unicode、基础渲染 | 能稳定编辑并保存普通项目 |
| P0 | 搜索、文件树、配置、命令注册、快捷键、测试 harness | 核心工作流可自动化验证 |
| P0 | TextMate 高亮、LSP 生命周期、诊断、补全、格式化 | 至少一个语言服务端到端可用 |
| P1 | Git 状态/暂存/提交/diff/blame/分支、PTY 终端 | 日常开发不需离开 mtide |
| P1 | 主题、设置 UI、Markdown、输出面板、AI Agent 面板 | 用户可配置并观察后台任务 |
| P2 | Lua 插件、权限模型、社区注册表、GitHub PR review | 生态与代码审查工作流可用 |
| P3 | 调试器、测试资源管理器、远程开发、Profile/Sync | 后续扩展，不阻塞首个稳定版 |

## 3. 功能需求

### FR-01 启动与工作区

支持当前目录、文件、目录、多根目录、`file:line:column`、stdin、`.ttt` 工作区和 GitHub Pull Request URL。活动文件按最长路径匹配工作区根；Explorer、Search、Changes 共享同一上下文。

### FR-02 编辑器

支持插入/删除/换行、复制剪切粘贴、查找替换、行操作、折叠、多光标、括号匹配、自动缩进、`.editorconfig`、格式化、鼠标和系统/OSC52 剪贴板。所有改变缓冲区的操作必须进入撤销系统。

### FR-03 语言服务

支持 stdio LSP：懒启动、初始化、全文同步、诊断、补全/解析、签名帮助、悬停、定义/实现/类型定义/引用、重命名、格式化、代码操作。服务异常必须可见、可重启，不得阻塞 UI。

### FR-04 文件与搜索

Explorer 支持多根树、排序、创建、重命名、删除、刷新和自动 reveal。跨工作区搜索调用 ripgrep，支持 smart-case、include/exclude glob、结果分组和跳转。

### FR-05 Git

支持仓库发现、状态、stage/unstage、commit、branch、pull/push/sync、diff、blame、提交历史、工作树可编辑 diff 和 PR review。多根工作区按仓库分组。

### FR-06 终端

支持多 PTY 标签、VT 解析、scrollback、ANSI/256/真彩色、终端链接和焦点直通。PTY 输出通过事件唤醒主循环，禁止后台线程直接修改 UI 状态。

### FR-07 UI 与配置

支持编辑区、侧栏、底部面板、右侧 Agent、overlay/dialog/menu/tabs/tree/table/list/input/markdown 等组件；所有颜色通过主题系统管理，终端真彩色是明确例外。

![image-20261104014528886](/home/macro/.config/Typora/typora-user-images/image-20261104014528886.png)

### FR-08 插件与 Agent

插件可注册命令、快捷键、面板、Widget、状态栏、文件处理器并访问受权限保护的文件、网络、终端、存储和定时器 API。Agent 为每个供应商维持独立持久 PTY 会话。

### FR-09 近期兼容增强

- 支持 bounded soft wrap；每种语言可覆盖 wrap column；启用换行时 Up/Down 按屏幕行移动。
- 支持可选括号/引号自动闭合、selection color invert、手动语言模式、水平滚动、上下添加光标和多光标剪贴板 spread。
- 支持运行中编辑器复用（`reuseWindow`）与 `--new-window`；支持保存/清除默认启动 folders，显式 targets 优先。
- 查找栏必须继承选区、支持 Find/Replace 一键切换，overlay 关闭恢复原 focus，粘贴按当前 focus 路由。
- 保存 dirty 状态以“当前文档内容是否等于保存基线”为准；undo/redo 不能错误丢失未保存提示。
- Git diff 解析兼容省略 count=1 的 hunk header；缩进检测平票结果确定；Toggle Comment 使用语言语法。

### FR-10 P2 规划能力

- 内联 merge conflict 解决：识别 ours/theirs/base 区块、逐块选择、保存前检查未解决冲突。
- Markdown 图片粘贴：读取剪贴板图片，写入受控资源目录，插入相对 Markdown 链接。
- VCS backend 抽象，未来支持 jujutsu (`jj`)；Git UI 不得依赖 Git 特有命名。

## 4. 非功能要求

- 单一可执行文件，Linux/macOS/FreeBSD/Windows/Android Termux 为目标平台。
- UI 线程无阻塞 I/O；所有异步结果可取消或丢弃过期结果。
- 正确处理 UTF-8、宽字符、组合字符和终端边界。
- 大文件有明确阈值和降级策略；默认 word-wrap 上限 32 MiB，自动重载上限 64 MiB。
- 核心算法可脱离终端测试；真实二进制行为可通过脚本复现。
