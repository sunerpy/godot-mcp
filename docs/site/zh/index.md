---
layout: home
title: godot-mcp：面向 Godot 项目的 MCP 服务器
titleTemplate: false
description: godot-mcp 是一个静态 Rust 二进制，把 Godot 项目交给 MCP 客户端：不需要引擎的分析、写入日志的改动，以及它自己运行、操控、截取和调试的游戏与编辑器。

hero:
  name: godot-mcp
  text: 面向 Godot 项目的 MCP 服务器
  tagline: 一个跑在项目旁边的静态二进制。你的 Agent 不用引擎就能读懂 GDScript、场景和资源，通过写入日志的事务修改它们，还能运行、操控、截取和调试游戏与编辑器。
  actions:
    - theme: brand
      text: 安装
      link: /zh/guide/install
    - theme: alt
      text: 快速开始
      link: /zh/guide/quick-start
    - theme: alt
      text: GitHub
      link: https://github.com/sunerpy/godot-mcp

home:
  facts:
    - term: 运行方式
      text: 由 MCP 客户端启动的一个本地进程，跑在你的项目旁边，支持 Linux、macOS 和 Windows。默认走 stdio，不开端口，也不需要账号。
    - term: Godot
      text: 分析只读项目文件，不需要引擎。运行、测试和导出使用你指定的 Godot 4.4.0 或更新版本。

  visual:
    label: 与 godot-mcp 的一段 MCP 交互，对象是一个小型 Godot 项目。对 main.tscn 调用 scene.tree.get，答复列出它的五个节点及其类型和脚本，以及 Mob 的 body_entered 信号到根节点的确切连接。对 player/player.gd 里的 take_damage 调用 gdscript.references.get，答复列出一处推断出的调用，位于 main.gd 第 6 行。
    transcript:
      - kind: call
        text: scene.tree.get
      - kind: argument
        text: "scene: main.tscn"
      - kind: answer
        text: ".            Node         main.gd"
      - kind: answer
        text: "Player       Area2D       player/player.gd"
      - kind: answer
        text: "HUD          CanvasLayer  ui/hud.gd"
      - kind: answer
        text: "HUD/Message  Label"
      - kind: answer
        text: "Mob          RigidBody2D"
      - kind: answer
        text: "Mob.body_entered → ._on_mob_body_entered  exact"
      - kind: call
        text: gdscript.references.get
      - kind: argument
        text: "script: player/player.gd"
      - kind: argument
        text: "symbol: take_damage"
      - kind: answer
        text: "main.gd:6  player.take_damage()  calls  inferred"

  index:
    title: godot-mcp 能做什么
    intro: 下面每一项都是 Agent 可以调用的工具。标着「无需引擎」的只读项目文件，在没装 Godot 的机器上也能回答；其余的需要一个 Godot 可执行文件，其中一部分还需要 godot-mcp 自己启动的游戏或编辑器。
    groups:
      - name: 读懂项目
        items:
          - title: 项目与设置
            body: 项目根目录、名称和主场景、每一项设置、每一个文件；从项目内任意目录都能找到项目。
            status: no-engine
            link: /zh/guide/analysis#项目
          - title: GDScript
            body: 跨脚本的符号、引用、调用方与影响面，每条关系都标明有多确定。
            status: no-engine
            link: /zh/guide/analysis#gdscript
          - title: 场景与资源
            body: 文本格式 .tscn 与 .tres 的节点树、脚本、实例、分组与信号连接。
            status: no-engine
            link: /zh/guide/analysis#场景与资源
          - title: 依赖
            body: 一个场景或资源加载了什么、被什么加载，以及指向不存在目标的引用。
            status: no-engine
            link: /zh/guide/analysis#依赖
      - name: 修改项目
        items:
          - title: 文件与脚本
            body: 创建、修补、移动和删除，每次都对照被改内容的 SHA-256，过期的修改会被拒绝而不是被应用。
            status: no-engine
            link: /zh/guide/editing#文件与脚本
          - title: 场景与资源
            body: 节点、属性与连接的改动打成一批，作为一个写入日志的事务，经由 Godot 或已打开的编辑器执行。
            status: engine
            link: /zh/guide/editing#场景与资源
          - title: 设置与资产
            body: 主场景、自动加载、输入动作、插件和设置通过类型化操作修改；二进制资产可以分块上传。
            status: no-engine
            link: /zh/guide/editing#设置与资产
          - title: 编辑器
            body: godot_mcp 插件让正在运行的编辑器连进来；对已打开场景的改动会成为编辑器里一次可撤销的操作。
            status: engine
            link: /zh/guide/editor
      - name: 运行项目
        items:
          - title: 检查、导入与测试
            body: 由 Godot 自己检查 GDScript，运行导入流程，以及运行项目自带的测试并给出确切计数。
            status: engine
            link: /zh/guide/engine
          - title: 导出
            body: 用你这个 Godot 版本的确切导出模板导出一个预设，作为可以跟踪和取消的托管导出。
            status: engine
            link: /zh/guide/engine#导出
          - title: 运行游戏
            body: 运行主场景或任意场景，带窗口或无窗口，可以读日志和状态，直到你停止它。
            status: engine
            link: /zh/guide/sessions#运行游戏
      - name: 操控与调试
        items:
          - title: 输入
            body: 键盘、鼠标、输入动作和定时序列；按住的输入在保持时间结束或会话停止时一定会被松开。
            status: engine
            link: /zh/guide/sessions#发送输入
          - title: 运行时读取
            body: 读取运行中的场景树、属性、信号和 UI，不改动游戏。
            status: engine
            link: /zh/guide/sessions#读取运行中的游戏
          - title: 截取
            body: 截图和短录屏，以带 SHA-256 的产物返回。
            status: engine
            link: /zh/guide/sessions#截取
          - title: 调试器
            body: 通过编辑器的调试适配器设断点、看调用栈、作用域和变量，并取得语言服务器的诊断。
            status: engine
            link: /zh/guide/sessions#调试

  steps:
    title: 从安装到第一个答复
    items:
      - title: 安装
        body: 运行下方「安装」一节里对应平台的安装脚本，或者从 GitHub Releases 下载归档，安装脚本会用发布里的 SHA256SUMS.txt 核对归档。
      - title: 在客户端里注册
        command: claude mcp add godot-mcp -- godot-mcp serve --project /path/to/my-game
        body: 任何会启动进程的 MCP 客户端都用同一条命令。默认是只读的 core profile。
      - title: 问它关于项目的问题
        body: 问 Agent 某个场景或脚本。它会先调 capabilities.get 和 health.get 了解这里能做什么，再调用分析工具。
      - title: 需要时再放开权限
        command: --profile full
        body: editor 增加写入，runtime 增加运行和观察游戏。依赖引擎的工具需要把 godot_binary 指向你的 Godot。

  profiles:
    columns: [Profile, 增加的能力, 工具举例]
    rows:
      - cells: [core, 只读，什么都不改, project.info.get scene.tree.get gdscript.references.get]
      - cells: [editor, 写入与编辑器插件, file.patch scene.batch.apply editor.addon.install]
      - cells: [runtime, 运行并观察游戏, project.run.start input.sequence.send runtime.screenshot.capture]
      - cells: [full, 以上全部, core editor runtime]
    code: [0, 2]
    caption: 不在 profile 里的工具不会被列出。已列出的工具如果它需要的引擎、游戏或编辑器不在，就答复 unavailable 并说明缺的是哪一个。

  sessions:
    columns: [要做的事, 调用, 说明]
    rows:
      - cells: [运行游戏, project.run.start project.run.stop, 带窗口或无窗口，直到生命周期结束]
      - cells: [按键与点击, input.key.send input.mouse.send input.action.send input.sequence.send, 按住的一切都会被松开]
      - cells: [实时读取, runtime.tree.get runtime.properties.get runtime.signals.get runtime.ui.get, 不改动游戏]
      - cells: [截取, runtime.screenshot.capture runtime.recording.capture, PNG 或 APNG，作为产物返回]
      - cells: [调试, editor.session.start debug.command, 断点、调用栈与变量]
    code: [1]
    caption: 同时最多运行四个会话。每个会话只属于启动它的那个服务器；崩溃后新启动的服务器会结束遗留的会话。

  versions:
    columns: [Godot, CI 运行的内容, 平台]
    rows:
      - cells: [4.7.1, 编辑器检查、导出模板、显示检查、打包冒烟, Linux、macOS、Windows]
      - cells: [4.6.1, 编辑器检查、导出模板, Linux]
      - cells: [4.5.1, 编辑器检查、导出模板, Linux]
      - cells: [4.4.1, 编辑器检查、导出模板, Linux]
    code: [0]
    caption: 确切版本来自同一个锁文件，每个都用官方 SHA-256 校验。最低支持版本是 4.4.0。

  platforms:
    title: 平台
    intro: 每次发布都为下面每个平台提供一个归档，并在该平台上针对锁定的 Godot 版本构建和冒烟测试。
    columns: [平台, 归档, 游戏与编辑器]
    rows:
      - name: Linux x86_64
        cells: [x86_64-unknown-linux-musl，静态链接, X11 或 Wayland 下带窗口，或无窗口；由进程组、委派 cgroup 或容器适配器限定]
      - name: macOS Apple 芯片
        cells: [aarch64-apple-darwin, 已登录用户的会话里带窗口，或无窗口；由进程组或容器适配器限定]
      - name: Windows x86_64
        cells: [x86_64-pc-windows-msvc，crt-static, 已登录的桌面会话里带窗口，或无窗口；由 Job Object 限定]
    note: Intel 版 macOS、Linux arm64 和 Windows ARM64 没有现成归档，请从源码构建。

  privacy:
    title: 项目数据去了哪里
    intro: godot-mcp 只在你的机器上工作。除了在你配置 OIDC 时去取身份提供方的签名密钥，它自己不发起任何网络请求。
    sendsLabel: 去向
    modes:
      - name: 工具调用
        sends: 你的 MCP 客户端
        detail: 走 stdio 时，客户端与服务器通过两条管道通信。答复里只有这次调用要求的项目数据，不会发往别处。
      - name: HTTP（需要你打开）
        sends: 回环监听
        detail: trusted_local 只接受带本次启动密钥的请求；proxied_oidc 只接受经过认证的代理转发、并携带某一个用户令牌的请求。
      - name: 引擎
        sends: 你的机器
        detail: Godot 用你配置的版本在本地运行，处在服务器管理的进程边界之内。godot-mcp 从不下载引擎或导出模板。
      - name: 日志与截取
        sends: 本地文件
        detail: 诊断信息写到 stderr 或滚动日志文件，密钥和源码会被遮蔽。截图、录屏和导出结果作为产物留在磁盘上，直到保留期结束。

  scope:
    title: 它不做的事
    items:
      - 不执行任意 GDScript，也不调用任意引擎方法。任何 profile 里都没有这样的工具。
      - 不下载 Godot 或导出模板，只使用你安装好的版本和模板。
      - 不修改二进制 .scn 和 .res，而是请你先把它们另存为文本格式。
      - 不能让一个不可信的项目变得可以放心打开。加载项目就会运行它的代码。
      - 不是共享服务。一个服务器只为一个用户服务一个项目。
---

<HomeIndex />

<HomeSteps />

<SplitBlock proof="profiles">

## 决定它能做什么

profile 是服务器能对项目做的一切的上限。`core` 只读，什么都不改。`editor` 增加写入，每次写入都写进日志，并核对它替换的内容。`runtime` 增加运行和观察项目。无论哪个 profile，都没有执行任意代码的工具。

[Profile 与答复](guide/profiles.md) · [工具与资源](../../protocol/tool-inventory.md)

</SplitBlock>

<SplitBlock proof="sessions" flip>

## 它能玩、也能看着的游戏

游戏由 godot-mcp 自己启动，带一个它自己的探针，不改动项目里的任何东西。Agent 可以按键、读取实时场景树、截图；编辑器会话还能设断点、看调用栈。会话结束时，它按住的每一个输入都会被松开。

[游戏与编辑器](guide/sessions.md) · [托管会话](../../operator/managed-sessions.md)

</SplitBlock>

<SplitBlock proof="versions">

## 引擎，版本精确

项目支持的每个 Godot 版本都是锁文件里的一个确切补丁版本，带官方下载地址和摘要。CI 探测每个版本的编辑器并用它检查脚本，核对每套导出模板，并在各自平台上针对最新版本冒烟测试每个发布归档。

[运行引擎](guide/engine.md) · [Godot 版本](../../operator/version-matrix.md)

</SplitBlock>

<HomePlatforms />

<HomePrivacy />

## 安装

::: code-group

```sh [Linux 与 macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
```

```sh [源码]
git clone https://github.com/sunerpy/godot-mcp.git
cd godot-mcp && make dist
```

:::

[安装指南](guide/install.md)介绍各个归档、如何校验下载以及从源码构建。

<HomeScope />

## 反馈

问题和功能建议请提交到 [GitHub Issues](https://github.com/sunerpy/godot-mcp/issues)。godot-mcp 采用 MIT 许可证，与 Godot Engine 项目没有隶属关系。
