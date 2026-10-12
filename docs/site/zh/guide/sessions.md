# 游戏与编辑器

在 `runtime` 和 `full` profile 下，服务器会启动并持有长期运行的 Godot 进程：用 `project.run.start` 启动的**游戏会话**，以及用 `editor.session.start` 启动的**编辑器会话**。会话只属于启动它的那个服务器，只能通过一个不透明的 ID 来识别（`run_` 或 `editor_` 后跟 32 位十六进制数字）。包括导出在内，同时最多运行四个会话，每个会话最晚在它的 `lifetimeSeconds` 到期时结束——默认一小时，最长四小时。

## 运行游戏

`project.run.start` 运行主场景或 `scene`，并带上服务器自己的运行时探针，通过命令行传给 Godot：项目不会为此做任何改动，不会安装自动加载，也不会改任何设置。探针通过一条只属于这个会话的回环连接连回服务器之后，调用才会答复，所以拿到答复的会话已经可以使用。

`display` 决定游戏怎样绘制：`window` 绘制到服务器启动时所在的显示环境，`headless` 完全不绘制，默认的 `auto` 在有显示环境时等同于 `window`。截取需要窗口。在 Linux 上，Xvfb 这样的虚拟 X 显示就足够了；在 macOS 上，服务器必须运行在已登录用户的会话里；在 Windows 上，必须运行在已登录会话的可见桌面上。

`project.run.status` 报告会话的状态、场景、显示方式、限定方式和按住的输入；`project.run.logs` 分页读取它捕获的输出，运行中和结束后都可以读；`project.run.stop` 松开所有按住的输入，请游戏退出，游戏不退出就结束它。

## 发送输入

`input.key.send`、`input.mouse.send`、`input.action.send` 和 `input.sequence.send` 用来按下、松开或轻按按键、鼠标按钮和输入动作，以及移动鼠标；一个序列最多 64 步，步骤之间可以等待。按住的一切都会在这些时候被松开：保持时间结束时（`holdMs`，默认 500 毫秒）、被显式松开时、按下它的序列结束或被取消时、会话停止时，以及游戏失去服务器时。每次输入调用都写进日志并带 `idempotencyId`，所以重试的调用会从日志里取回结果，而不会再按一次。

## 读取运行中的游戏

`runtime.tree.get`、`runtime.properties.get`、`runtime.signals.get` 和 `runtime.ui.get` 通过探针读取游戏，不改动游戏：带类、脚本、分组和可见性的场景树，一个节点的属性（带类型的值），一个节点的信号和连接（设置 `watch` 后还会统计之后的每次发射），以及可见控件的矩形、文本和焦点。每个答复都写明探针的场景版本号和它取自哪一帧。

## 截取

`runtime.screenshot.capture` 把根视口截成一张 PNG；`runtime.recording.capture` 以最多每秒 30 帧录制最长 10 秒，生成一个动态 PNG。每一帧都对照探针报告的摘要校验，答复把截取结果作为一个产物——带媒体类型、大小和 SHA-256 的不透明 URI——客户端用 `resources/read` 读取。无界面会话不渲染画面，在其中截取会答复 `unavailable`。

## 调试

`editor.session.start` 在项目上启动一个无界面的 Godot 编辑器，它的 GDScript 语言服务器和调试适配器监听服务器分配的回环端口；只有当这两个端口都在监听、并且确实由这个编辑器进程持有时，调用才会答复。之后 `debug.command` 通过会话的启动凭据发送一条命令：

| 服务 | 命令 |
| --- | --- |
| 语言服务器 | `diagnostics`、`symbols` |
| 调试适配器 | `breakpoints`、`launch`、`stopped`、`stack`、`scopes`、`variables`、`continue`、`pause`、`stop` |

行号从 1 开始。没有表达式求值，也没有单步跳出，因为 Godot 的调试适配器没有实现它们。`debug.session.get` 报告连接、调试运行、最近一次停下以及断点；`editor.session.stop` 结束编辑器以及它在调试器下运行的游戏。

## 崩溃之后

正常停止的服务器会先停止它的会话。下一个为同一项目启动的服务器，会在提供任何服务之前结束前一个服务器遗留的所有会话，并把每个会话记为 `reconciled`；如果无法证明它的边界已经清空，就记为 `quarantined`。它们的状态和输出仍然可以读取。全部规则见[托管会话参考](../../../operator/managed-sessions.md)。
