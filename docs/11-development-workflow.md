# 开发流程

## 1. 开始任务

先写清楚用户可见行为、受影响模块、状态不变量和测试层级。检查是否改变命令 ID、快捷键、配置或插件 API；若是，先更新兼容说明。

## 2. 实现顺序

优先纯领域模型和 service trait，再接应用命令，最后接 widget/layout。每个外部边界先做 fake/mock 和错误路径，再接真实实现。

## 3. 提交要求

一个提交聚焦一个行为变化；提交信息说明模块和兼容影响。不要把重构、格式化和功能混在同一提交。新增命令需同步默认 keybinding 镜像、帮助和测试。

## 4. Review 清单

- 是否所有 buffer mutation 经过 undo？
- 是否混用了 byte/rune/visual 坐标？
- 是否后台线程直接触碰 UI？
- 是否处理取消、超时、旧结果和进程退出？
- 是否所有颜色来自 theme？
- 是否有确定性测试而非固定 sleep？
- 是否更新配置、插件、CLI 或文档契约？
- 是否在真实二进制中用 `--exec` 验证？

## 5. ADR 触发条件

以下变化必须新增 ADR：更换文本存储结构、改变事件循环/线程模型、改变 `.ttt` 或 settings schema、引入新的插件权限、改变终端后端、放弃与 ttt 的用户行为兼容。
# 上游 TTT 插件兼容烟测

准备 `ttt-plugins` 社区插件仓库并构建 mtide 后，可逐个加载插件入口，并将所有插件同时加载，验证 manifest、Lua 入口、所需宿主模块及聚合注册是否能初始化：

```sh
cargo build
scripts/verify-ttt-plugins.sh /path/to/ttt-plugins
```

也可通过 `TTT_PLUGINS_DIR` 指定插件仓库，通过 `MTIDE_BIN` 指定待验证的 mtide 可执行文件。脚本使用临时 HOME/XDG 配置目录，并在入口加载后立即退出。它验证插件启动兼容性，不会验证插件依赖的外部命令、网络服务和完整交互工作流。
