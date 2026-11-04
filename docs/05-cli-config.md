# CLI、配置与工作区文件

## 1. CLI

```text
mtide [options] [files|folders|urls...]
```

兼容参数：`--help/-h`、`--version/-v`、`--workspace FILE`、`--config FILE`、`--plugin DIR`（可重复）、`--exec SCRIPT`、`--exec-split-on SEP`、`--size WxH`、`--debug`、`--listen`、`--new-window`、`--welcome`、`-`。Rust 版当前实际处理 `--config`、`--plugin`、`--welcome`、`-v` 和标准输入目标。

位置参数支持文件、目录、`file:line[:column]` 和 `.ttt` 工作区；文件定位使用 1-based 行列并转换为内部 0-based 光标。无显式目标且 stdin 为 pipe/regular file 时，会在进入终端 raw mode 前自动读取并以未保存 tab 打开；显式 `-` 强制执行同样行为，适用于 `cat file | mtide -`。文件/目录目标和配置中的默认 folder 优先于自动 stdin。

`--exec` 用于无交互测试和复现，动作包括 `click`、`rclick`、`hover`、`drag`、`key`、`type`、`paste`、`copy`、`exec`、`screenshot`、`debug`、`wait`、`wait-for`、`panel`、`quit`。失败返回非零退出码。

## 2. 配置目录与文件

配置目录由平台默认值决定，可用 `TTT_CONFIG_DIR` 等价环境变量覆盖。建议 Rust 版使用 `directories` crate，但保留文件名和 JSON shape：

- `settings.json`：编辑器、终端、LSP、Git、搜索、外观、Agent 等；`theme` 可指定主题 JSON 路径；`defaultFolders`（别名 `default_folders`/`startupFolders`）配置无显式目标时的默认启动 folders。
- `keybindings.json`：用户覆盖；默认值代码内生成并导出镜像。
- `themes/*.json`：主题；启动时会收集内置主题、`settings.json` 的 `theme` 路径和 `--plugin` 包内主题。
- `workspace.ttt`：可选默认启动 folders，不保存打开文件和面板布局。
- `ttt.log`/`mtide.log`、`crash.log`：debug 与崩溃信息。

加载顺序：内置默认值 → 配置文件 → 迁移旧字段 → normalize/validate → 运行时覆盖。未知字段应保留兼容容忍，非法值回退并产生诊断。

启动目标优先级：显式文件/目录/`.ttt`/`--workspace`/`-` > `--welcome`（欢迎缓冲区）> `defaultFolders` 第一项 > 自动 stdin > 当前目录；默认目标无效时回退欢迎缓冲区并保留错误状态。

## 3. 关键 settings

编辑器包括 `tabSize`、`insertSpaces`、`language`、`wordWrap`、`wordWrapMaxColumn`、`lineNumbers`、`formatOnSave`、`insertFinalNewline`、`trimTrailingWhitespace`、`syntaxHighlight`、`gitGutter`、`diffMode`、`diffContext`、`gutterStyle`、`borderStyle` 和 large-file limits。当前 Rust 版已读取 `editor.language` 作为全局语言回退，并用于状态栏、注释语法、语法着色和 LSP server 选择；编辑器中按 `Ctrl-Alt-L` 可对当前 tab 循环设置语言覆盖，并写入工作区 `.mtide/language-overrides.json`，下次启动或重新打开文件时恢复；未设置 tab 覆盖时依次回退到全局 `editor.language` 和文件扩展名。已读取 `editor.wordWrap`、`editor.wordWrapMaxColumn` 并用于编辑区渲染；启用 `formatOnSave` 时保存前请求 LSP formatting，保存时再按 `insertFinalNewline`/`trimTrailingWhitespace` 规范化文本。主题 JSON 支持 `colors` 对象及语义色，颜色使用 `#RRGGBB` 或 `#RGB`；插件主题包启动时会自动加载首个有效主题。

搜索配置位于顶层 `search`：`include` 和 `exclude` 为 glob 字符串数组；`include` 为空表示不限制文件类型，`exclude` 可使用 `target/**`、`node_modules/**` 或 `*.generated.*` 等规则。默认排除 `target`、`node_modules`、`vendor`，隐藏目录仍不会被递归扫描。规则同时作用于多根工作区搜索和跨文件替换。

近期兼容配置还包括 `reuseWindow`、`autoClosingBrackets`、`selectionStyle`/selection color invert、`wordWrapMaxColumn` 及按语言覆盖、`language` override、`saveAllOnQuit`、`terminal.ctrlV` 和 quick-open 的独立 hidden/gitignored visibility。启用 `saveAllOnQuit` 后退出会先保存所有 dirty 标签，任一保存失败则取消退出并保留错误状态。配置迁移必须保留未知字段并支持默认值。

编辑器焦点下 `Ctrl-O` 打开 Open File 路径弹层；相对路径相对于当前工作区根目录，Enter 打开文件，Esc 取消。`Ctrl-Shift-S` 打开 Save As，保存到新路径后更新当前 tab，已有路径不会被覆盖；`Ctrl-G` 打开 Go to Line，输入 `line` 或 `line:column` 定位；`Ctrl-Backspace`/`Ctrl-Delete` 删除光标前/后的一个词（必要时跨越行边界），选区存在时仍删除选区；检测到脏文件发生外部变更后，`Ctrl-Alt-R` 显式从磁盘重载并清空该标签撤销栈；`Ctrl-Alt-L` 循环当前 tab 的语言覆盖。`Ctrl-Alt-T` 在已加载主题间循环切换。LSP 快捷键包括 `Ctrl-Space` 补全、`Ctrl-H` 悬停、`Ctrl-Alt-S` 签名帮助、`Ctrl-D` 定义。

终端包括 `terminal.shell`、`terminal.args`、`terminal.scrollback` 和 `terminal.ctrlV`；`ctrlV` 默认关闭，开启后将内部剪贴板发送到当前 PTY。搜索包括 `debounce`（原项目默认 350ms）。LSP 包括 enabled、hover、servers、saveOnRename、codeActionsOnSave、hoverDelay（默认 500ms）。Agent 当前支持 `agent.enabled`、兼容旧格式的 `agent.name`/`agent.command`/`agent.cwd`，以及 `agent.active` 和 `agent.providers` 多 Provider 配置；每个 Agent 使用独立 PTY。面板焦点下按 `[`/`]` 切换 Provider。自动补全默认启用，debounce 默认 150ms。

## 4. `.ttt` 工作区

```json
{
  "folders": [
    { "path": "." },
    { "path": "../other-project" }
  ]
}
```

路径相对 `.ttt` 文件所在目录解析；加载后规范化但保存时尽量保持相对形式。多个 folder 会聚合到 Explorer 和 workspace search；Git 操作默认使用第一个 folder 作为活动仓库上下文。无效 root 必须显示错误而不是静默丢弃。

## 5. 安全

`--listen` 只绑定 loopback；插件、外部命令、网络和文件操作均需要明确权限或配置。日志不得记录 API key、完整环境变量或敏感文件内容。

多根 `.ttt` 工作区的 Explorer 在根目录视图聚合各工作区条目，并以 `根目录/相对路径` 显示，刷新后保持聚合视图。
## CLI

```text
mtide [OPTIONS] [FILE|DIRECTORY]
    --plugin DIR       可重复，加载一个 TTT 兼容 Lua 插件包
    -h, --help
    --version
```

侧栏快捷键：`1` Explorer、`2` Find、`3` Changes、`4` Branches；Explorer 中 `n` 新建文件、`Shift-N` 新建目录、`r` 重命名、`x` 删除（需 `y` 确认）、`F5` 刷新；Branches 中 Up/Down 选择、Enter 切换分支。编辑器中 `Ctrl-Shift-[` 折叠当前缩进块，`Ctrl-Shift-]` 展开全部。

