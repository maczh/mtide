# Git 远程凭据交互

fetch、pull、push 默认关闭 Git 终端交互，避免阻塞 TUI。远程操作因认证失败时，mtide 打开凭据 overlay：Tab 在用户名和密码之间切换，Enter 使用当前输入重试，Esc 取消。

凭据只保存在当前进程内存中。重试时通过一次性 Git credential helper 的进程环境变量传递给 Git，不写入 `.git`、配置文件、日志、状态栏或工作区；操作结束后 prompt 状态立即清理。

该流程不替代系统级 credential helper、SSH agent 或操作系统密钥链。若用户已经配置这些机制，Git 成功认证时不会显示 overlay。
