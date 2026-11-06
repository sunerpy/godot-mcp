---
description: 不启动 Godot 就能读取项目、GDScript、场景、资源和依赖关系的工具。
---

# 静态分析

本页的工具只读项目文件，不读别的。它们在所有 profile 下都能用，在没装 Godot 的机器上也能工作，并且从不启动任何进程。

## 项目

| 工具 | 回答 |
| --- | --- |
| `project.info.get` | 项目的标识、设置摘要以及工作区版本号 |
| `project.discover` | 每个脚本、场景、资源和其他文件，以及它们的种类 |
| `project.files.list` | 全部文件，或某个前缀下、匹配某个 glob 的文件，带种类、大小和 SHA-256——之后的写入要用到这个哈希 |
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

## 一次看整个项目

| 工具 | 回答 |
| --- | --- |
| `project.dependencies.audit` | 项目里没有任何入口能到达的文件，并逐个说明原因；`view: "cycles"` 时给出互相引用成环的文件，以及 Godot 遇到这种环会怎样 |
| `project.signals.get` | 脚本声明的每个信号，连接它的场景和脚本，以及发出它的代码行；`view: "problems"` 时给出连到不存在方法的连接、发出却没人连接的信号，以及连到未声明信号的连接 |
| `project.metrics.get` | 按种类和扩展名统计的文件数与字节数，以及 GDScript 的代码行、注释行和空行；`view: "complexity"` 时按复杂度、长度或嵌套深度给函数排序，按节点数、深度或实例数给场景排序 |
| `project.search` | 匹配一个字面字符串或正则表达式的每一行，可附带上下文行 |
| `project.uid.get` | 某个 `uid://` 指向的文件，或某个文件的 UID，依据项目里的场景文件头、`.uid` 文件和 `.import` 文件 |
| `scene.nodes.find` | 所有场景里按类型、分组、名称、脚本或已存属性找到的节点，包括实例化进来的场景 |
| `scene.exports.get` | 一个场景里挂了脚本的节点的导出变量，带脚本里的默认值和场景存下的值 |

未使用文件的列表是排查的起点，不是结论：运行时才拼出路径的脚本，比如 `load("res://levels/" + name + ".tscn")`，能加载没有任何地方写出名字的文件。答复会列出所有这类调用；位于某个调用字面前缀之下的未使用文件，会标出可能加载它的调用。

Godot 编辑器会跳过隐藏的文件和文件夹，以及含有 `.gdignore` 的文件夹；这些工具跳过同样的文件，每个答复都会写明跳过了多少。场景里连到接收方没有的方法的连接会被列为问题，因为 Godot 自己不会提示：场景照常加载，信号触发时也不报错。只记录在 Godot 的 `.godot/` 缓存里的 UID 会答复 `unknown`，而不是“不存在”。

`include` 和 `exclude` 接受 glob：`*` 和 `?` 不跨文件夹，`**` 可以跨文件夹，不含 `/` 的模式在任意文件夹里匹配文件名，比如 `*.tscn`。每个视图、字段和上限见[项目级分析参考](../../../operator/project-analysis.md)。

## 读懂答复

每个分析答复都带同样的外层信息：项目的标识与代号、工作区版本号以及索引是否一致、答复的来源（`source` 与 `confidence`），列表类答复还有一页 `limit`、`offset` 和 `next_offset`。
