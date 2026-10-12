# 静态分析

本页的工具只读项目文件，不读别的。它们在所有 profile 下都能用，在没装 Godot 的机器上也能工作，并且从不启动任何进程。

## 项目

| 工具 | 回答 |
| --- | --- |
| `project.info.get` | 项目的标识、设置摘要以及工作区版本号 |
| `project.discover` | 每个脚本、场景、资源和其他文件，以及它们的种类 |
| `project.files.list` | 全部文件或某个前缀下的文件，带种类、大小和 SHA-256——之后的写入要用到这个哈希 |
| `project.settings.get` | 类型化的设置：应用名称、主场景、自动加载、输入动作及其事件、已启用的插件，以及 `project.godot` 的 SHA-256 |
| `project.validate` | 整个项目的解析错误、悬空引用和无效设置，并说明没有运行引擎 |

答复来自项目文件的一个一致快照，并写明它取自哪个工作区版本号；一次写入提交后，版本号会前进。

## GDScript

| 工具 | 回答 |
| --- | --- |
| `gdscript.symbols.get` | 一个脚本声明的类、函数、信号、变量和常量 |
| `gdscript.references.get` | 对一个脚本，或对它声明的某个函数、信号或类名的引用和调用方 |
| `gdscript.impact.get` | 依赖某个脚本或符号的场景、资源和脚本，传递地展开 |
| `gdscript.diagnostics.get` | 解析器与分析给出的诊断，针对整个项目或某一个脚本 |
| `script.validate` | 对一个脚本做静态检查，并说明没有运行引擎 |

每条关系都带 `confidence`：代码直接点名目标时是 `exact`，通过有类型的变量或类名解析到目标时是 `inferred`，无法确定时是 `uncertain`——这时答复会列出所有候选，而不是挑一个。答复还会写明自己的局限：GDScript 用字符串指定节点或信号（`get_node("Path")`、`connect("hit", …)`），这类关系无法追踪时，答复会明说，而不是悄悄把它漏掉。

```text
gdscript.references.get  script: player/player.gd  symbol: take_damage
→ main.gd:6  player.take_damage()  calls  inferred
```

要让引擎本身检查脚本，请用 `script.check`，它运行 Godot 自己的 `--check-only`（见[运行引擎](engine.md)）。

## 场景与资源

`scene.tree.get` 读取文本格式的 `.tscn` 文件：每个节点的类型、父节点、分组、脚本和实例化的场景，信号连接及其方法，文件的 SHA-256，以及每个节点的 `handle`——在文件改变之前，`scene.batch.apply` 都接受它。`resource.tree.get` 以同样的方式读取文本格式的 `.tres` 文件：它的类型、属性、子资源和引用。

二进制 `.scn` 与 `.res` 文件答复 `unsupported`，并附上补救办法：在 Godot 里把文件另存为文本格式。服务器不会去猜二进制格式。

## 依赖

| 工具 | 回答 |
| --- | --- |
| `project.dependencies.get` | 项目里脚本、场景和资源之间的每条依赖关系，无论已解析还是悬空 |
| `scene.dependencies.get` | 一个场景依赖的脚本、子场景和资源，以及每一项如何解析 |
| `resource.dependencies.get` | 一个资源的外部依赖和子资源依赖，以及每一项如何解析 |

目标不存在的关系会被报告为悬空，而不是被丢弃；`project.validate` 会列出项目里每一个悬空引用。

## 读懂答复

每个分析答复都带同样的外层信息：项目的标识与代号、工作区版本号以及索引是否一致、答复的来源（`source` 与 `confidence`），列表类答复还有一页 `limit`、`offset` 和 `next_offset`。
