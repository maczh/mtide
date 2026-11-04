# Rust 重构路线图

## 阶段 0：文档与基线

冻结命令 ID、配置 key、`.ttt` schema、核心快捷键和代表性 `--exec` 场景。为原项目建立行为样例，尤其是 Unicode、undo、diff、terminal focus。

## 阶段 1：可运行骨架

建立 Cargo workspace、错误体系、事件循环、模拟 screen、cell grid、CLI help/version、配置加载和最小 App。交付：打开空界面、退出、resize、确定性 screenshot。

## 阶段 2：核心编辑器

实现 Buffer/Position/Selection/Undo/Fold/MultiCursor、文件读写、tab、基础 widget 和双缓冲渲染。交付：P0 编辑功能与 unit/E2E 覆盖。

本阶段同时锁定 save baseline dirty 计算、多光标 clipboard batch、soft-wrap screen-row mapping、horizontal viewport、auto-closing 和语言 comment syntax；这些是近期上游稳定性反馈，不应留到 UI 完成后补。

## 阶段 3：工作区与搜索

实现多根 workspace、Explorer、watcher、quick open、ripgrep search、`.ttt` 文件。交付：跨根文件和搜索可用。

## 阶段 4：配置、主题与语言服务

实现 settings/keybindings/theme/editorconfig、TextMate 或等价高亮、JSON-RPC/LSP。交付：至少一个语言的诊断、补全、跳转和格式化。

## 阶段 5：Git、diff、终端

实现 Git backend、changes panel、diff overlay、blame；再接入 PTY/VT、多终端和链接。交付：完整日常开发闭环。

diff parser 先覆盖省略 count=1，terminal 先覆盖 application cursor mode、幂等 close、Ctrl+V 配置和 resize background-job 场景。

## 阶段 6：插件与 Agent

实现 Lua host、manifest、permission、Widget API、社区安装和右侧 Agent。交付：示例插件、拒绝权限测试和持久会话。

插件第二批补齐 tabs/pager、diff navigation、sys.spawn、异步 exec、row actions、horizontal table scroll、theme manifest 和资源清理。

插件第一批运行时已落地：Lua 5.4 嵌入、manifest API 版本校验、`ttt` 核心注册/日志/通知、JSON 和权限保护的 system exec；后续继续将注册信息映射为真实 Widget 和命令事件。

## 阶段 7：兼容性与发布

完成 PR review、安装包、跨平台构建、性能基准、文档站迁移和 release checklist。

## 每阶段完成标准

代码、测试、用户行为样例、错误处理、日志/可观测性、文档同步完成后才算阶段结束。不要在核心模型未稳定时先堆 UI 功能。

## 高风险事项

1. Unicode visual width 与终端真实绘制不一致。
2. 大型 editor widget 造成 core 与 UI 强耦合。
3. 异步结果覆盖新文档或新搜索结果。
4. PTY/VT 细节被高级终端行为放大。
5. Lua API 与 Rust 生命周期/错误模型不匹配。
6. Git diff 可编辑视图的行映射在编辑后失效。
7. undo 栈与 dirty baseline 脱钩导致数据丢失。
8. overlay/focus 与终端/插件输入的剪贴板路由错误。
