# 上游 TTT 近期功能与社区建议研究

研究日期：2026-11-04。资料来源为原版仓库、v1.10.0 release、当前 Issues 和 Pull Requests 页面。本文区分已发布功能、已提出但尚未确认合入的建议，以及 Rust 重构的纳入决策。

## 1. 已发布功能：必须纳入兼容基线

原版 v1.10.0（2026-10-30）明确包含：可选括号/引号自动闭合、多光标复制/剪切/粘贴、限定列软换行及语言覆盖、运行中编辑器复用、Markdown details 折叠、插件 tabs/pager/密码输入/表格行操作/水平滚动/文本选择复制、插件 `sys.exec` 超时取消与异步输出、插件 diff tab 导航、可配置终端 Ctrl+V、插件贡献主题、Open File 命令和 LSP 批量编辑单步撤销等。[v1.10.0 release](https://github.com/eugenioenko/ttt/releases)

这些能力升级为 P0/P1 兼容需求：

- 编辑器：soft wrap、screen-row 上下移动、auto-closing、selection style、multi-cursor clipboard、language override。
- 启动：reuse running window、`--new-window` 优先级、Open File 命令、默认工作区。
- 插件：tabs/pager、可访问 diff tabs、row actions、table selection、horizontal scroll、`ttt.text` 宽度、`sys.spawn`、`sys.exec_async`、theme manifest。
- 稳定性：搜索结果随编辑更新、异步替换可取消、PTY 只关闭一次、终端 application cursor mode、Save As 覆盖保护、LSP edit batch 单次撤销。

## 2. 高价值 Issues

截至研究日，Issues 页面显示 50 个 open issues；近期高价值条目包括：

| Issue | 建议纳入 |
|---|---|
| [#1314](https://github.com/eugenioenko/ttt/issues/1314) undo 可能错误清除 dirty 状态并丢弃编辑 | P0：dirty 必须由文档内容/保存基线计算，不能只由 undo 栈为空判断 |
| [#1305](https://github.com/eugenioenko/ttt/issues/1305)、[#1304](https://github.com/eugenioenko/ttt/issues/1304)、[#1293](https://github.com/eugenioenko/ttt/issues/1293) 查找栏焦点与粘贴路由 | P0：focus 栈、overlay 关闭恢复、Ctrl+V 必须路由到当前输入框 |
| [#1303](https://github.com/eugenioenko/ttt/issues/1303)、[#1302](https://github.com/eugenioenko/ttt/issues/1302) 选中文本进入 Find、Find/Replace 一键切换 | P1：搜索 overlay 继承 selection，并允许无损切换模式 |
| [#1292](https://github.com/eugenioenko/ttt/issues/1292) 上下添加光标快捷键 | P1：增加 add cursor above/below 与多光标可见性测试 |
| [#1283](https://github.com/eugenioenko/ttt/issues/1283) 手动选择语法高亮语言 | P1：文件语言状态可覆盖扩展名探测并持久化到 tab/session |
| [#1272](https://github.com/eugenioenko/ttt/issues/1272) Markdown 粘贴剪贴板图片为文件并插入链接 | P2：剪贴板图片、资源目录策略和 Markdown 插入模板 |
| [#1261](https://github.com/eugenioenko/ttt/issues/1261) 水平滚动命令与滚轮支持 | P1：软换行关闭时支持 horizontal viewport |
| [#1125](https://github.com/eugenioenko/ttt/issues/1125) jujutsu (`jj`) VCS | P2：抽象 VCS backend，不把 Git 假设泄漏到 UI |

## 3. 高价值 PR

近期 PR 页面列出以下值得吸收的设计：

- [#1239](https://github.com/eugenioenko/ttt/pull/1239)：diff hunk header 缺少 count=1 时仍能解析。Rust diff parser 必须覆盖省略数量的合法 unified diff。
- [#1238](https://github.com/eugenioenko/ttt/pull/1238)：缩进检测平票时保持确定性。实现 tie-break 规则，不能依赖 map/线程顺序。
- [#1123](https://github.com/eugenioenko/ttt/pull/1123)、[#1077](https://github.com/eugenioenko/ttt/pull/1077)：LSP completion/format/rename/workspace edit 作为一个 undo batch。
- [#1082](https://github.com/eugenioenko/ttt/pull/1082)：overlay 关闭后返回原 focus；focus manager 应保存/恢复栈，而不是硬编码 editor。
- [#1021](https://github.com/eugenioenko/ttt/pull/1021)：保存默认启动 folders，显式文件/目录/PR/`--workspace`/`--welcome` 优先；坏文件回退普通启动。该 PR 页面当时仍为 open，因此 mtide 作为建议兼容项实现并注明状态。
- [#1019](https://github.com/eugenioenko/ttt/pull/1019)：编辑器内联解决 merge conflict，纳入 P2 设计。
- [#1007](https://github.com/eugenioenko/ttt/pull/1007)：Toggle Line Comment 使用语言 comment syntax，而不是固定 `//`。
- [#948](https://github.com/eugenioenko/ttt/pull/948)：多光标复制/剪切/粘贴按文档顺序、整行 fallback、匹配行数 spread，并作为单次 undo；该 PR 说明了重叠范围必须先归并，避免误删。
- [#1318](https://github.com/eugenioenko/ttt/pull/1318)：Control/Super 快捷键兼容；mtide 以 `keyboard.modifierKey` 提供 `control`、`control+super` 和 `super` 模式，并仅在 Host 快捷键匹配层将 Super/Cmd 归一化到现有 Control 快捷键；终端、Agent 与插件事件保留原始修饰键。

## 4. 纳入规则

- v1.10.0 release 中已列出的功能：直接进入兼容基线，不标记为未来功能。
- Issues/PR 中明确修复可靠性、数据安全、焦点或坐标一致性的建议：进入 P0/P1，即使上游尚未合入。
- 新的大功能（jj、内联冲突解决、图片粘贴）进入 P2，先定义 service/状态边界。
- 纯视觉偏好（tab 截断、overflow 图标、picker 间距）进入 UI backlog，不阻塞核心编辑器。

## 5. 研究限制

GitHub 页面会随时间变化，Issues/PR 的 open 状态不等于建议会合入上游；本文件只记录研究日可见信息。每次开发周期应重新检查上游 release、issues 和 pulls，再更新本文件。
