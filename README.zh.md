# filesnap

[![crates.io](https://img.shields.io/crates/v/filesnap.svg?label=filesnap)](https://crates.io/crates/filesnap)
[![crates.io](https://img.shields.io/crates/v/filesnap-cli.svg?label=filesnap-cli)](https://crates.io/crates/filesnap-cli)
[![npm](https://img.shields.io/npm/v/filesnap.svg?label=npm)](https://www.npmjs.com/package/filesnap)
[![CI](https://github.com/extracurricular-ai/filesnap/actions/workflows/ci.yml/badge.svg)](https://github.com/extracurricular-ai/filesnap/actions/workflows/ci.yml)
[![Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[English](README.md) | 中文

不依赖 git 的文件快照与回退:一个内容寻址的存储,把目录还原成它先前某一刻的样子。

它在从没 `git init` 过的目录里能用,在用着 git 的目录里也能用 —— 后者它只把索引当作一份文件*名*清单来读,一个字节都不写回去。你的 commit、你的 stash、你的工作区状态,一概不动;也不会往你的仓库里存任何东西。

它是为"会改文件、需要一个用户敢信的撤销"的 agent 写的,但引擎本身没有任何一处是 agent 形状的:它只接受不透明的字符串 id 和绝对路径,对"一次会话"是什么毫无意见。

```console
$ filesnap capture --session s1 --turn t1
{"v":1,"type":"capture.done","manifest":"a5a2b149…","reused":0,"hashed":1,"dropped":0}

$ echo "something regrettable" > a.txt

$ filesnap restore --session s1 --turn t1
{"v":1,"type":"restore.done","written":1,"deleted":0,"failed":0,"safety":"e41358f0…"}
```

那个 `safety` id,就是这次回退本身可以被回退到的点。每一次还原动手写第一个字节之前,都会先抓一个。

## 给 coding agent 加回退

回退是两个问题,而这个引擎只解决其中一个。

**每个 agent 都一样的那一半。** 在一轮开始时捕获工作区的样子,需要时把它放回去;项目可以没有仓库,代价不随 checkout 的大小增长。一个二进制 —— 六个发布构建在 3.4 到 4.2 MB 之间 —— 不带自己的运行时,而它前面是一次以秒计的模型请求。

**不一样的那一半。** 一个回退点在对话里处于什么位置、哪一轮值得捕获、文件动了以后 transcript 怎么办、两者按什么顺序发生才能让中间崩溃仍然可恢复。filesnap 拒绝替你决定其中任何一件:引擎不提供任何 hook,CLI 也没有 `rewind` 命令 —— 叫这个名字的命令会承诺那个合并操作,却只交付它的一半。两半由宿主来排序,因为宿主是唯一同时握着两边的那一层。

两条路:

- **Rust** —— `cargo add filesnap`。`WorkspaceStore::open` 加一个 `TurnScope`,然后在一轮开头调 `capture_turn`、在每次写入之前调 `declare_edits`;`target_for_turn` 把一个 turn id 解析成 `RestoreTarget`,`restore_to` 把它应用下去。`scan_report` 回答"什么没被覆盖"。本仓库的 CLI 就是参考实现,[`crates/filesnap-cli/src/commands/`](crates/filesnap-cli/src/commands) 下一条命令一个模块,而 [`restore.rs`](crates/filesnap-cli/src/commands/restore.rs) 就是这套顺序本身。
- **其他任何语言** —— 直接驱动那个二进制。每条命令都往 stdout 写带版本号的 JSON Lines,给人看的文字一律走 stderr,退出码也是契约的一部分 —— `2` 表示这条命令没能跑起来、或者跑了但报不出来,所以终结事件不会出现在 stdout 上 —— 于是整个集成就是"起个子进程 + 按行解析"。`npm install filesnap` 装到的是二进制,没有 JS API:这本来就是预期路径,不是退路。

一轮,以及一次回退进 fork 出来的会话:

```console
$ filesnap capture --session s1 --turn t3
$ filesnap declare --session s1 --turn t3 --path /abs/src/main.rs
$ filesnap restore --session s1 --turn t3 --undo-for s2
$ filesnap undo    --session s2
```

`declare` 紧贴在你的工具写入之前跑,它才是够得着"两个扫描分区够不着的那些文件"的东西 —— 只上 `capture` 和 `restore`,集成看起来是对的,而那些文件在悄悄地漏。`--undo-for` 指定撤销记录归档到哪个会话,所以它必须是用户最终所处的那个;不带它的还原是**故意**不可撤销的。id 由你来定,只有一条规则是存储会拒绝而不是替你修的:`[A-Za-z0-9._-]`、最多 200 字节、不能以 `_` 开头。一个会话只和它自己串行,不会更宽。一段对话结束时,`filesnap delete --session` 把它删掉、`filesnap gc` 回收没人再引用的东西;`filesnap doctor` 清理一次被打断的操作留下的残留。

## worked example:dsh-filesnap

[**dsh-filesnap**](https://github.com/extracurricular-ai/dsh-filesnap) 是 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的回退与撤销,以插件形式发布:`/rewind` 列出当前会话能回到的那些轮次,选中一轮就在那里 fork 对话并把文件一起带回去,`/redo` 反向撤销这次回退。它是 TypeScript 写的,把这个二进制当子进程驱动 —— "驱动那个二进制"在现实里到底要花多少,它就是答案。

**它通向本引擎的整条传输层,就是 [`src/cli.ts`](https://github.com/extracurricular-ai/dsh-filesnap/blob/main/src/cli.ts) 的 116 行(不含注释)。** 起进程、解析 JSON Lines、映射退出码。那个文件里没有一处知道"回退"是什么:没有 turn、没有 session、没有对话。它与宿主唯一的耦合是"用什么去起进程"这一处,那部分你要改写;其余部分就是本引擎的契约,可以原样抄走。

那个插件宿主半边另外约 1,100 行,才是 filesnap 拒绝去做的那一半,也就是这次集成实际花掉的东西:在 `agent/pre-step` 上捕获,早于模型请求、也早于任何工具运行;在写入和编辑的 seam 上声明 pre-image,那是旧字节还存在的最后一刻;先 fork 再把文件还原**进那个 fork**,因为撤销记录必须落在用户最终所处的那个会话里。

捕获的代价,在那个项目里实测:

| | 捕获的文件数 | 首次捕获 | 之后每次 |
|---|---|---|---|
| 插件自己的仓库 | 84 | 20 ms | 8 ms |
| harness 主仓 | 磁盘上 70,918 个,捕获 7,995 个 | 1.75 s | 268 ms |

*(在插件作者的机器上用 `filesnap capture` 实测,page cache 热态。你的数字会不一样,但形状是一样的。)*

**捕获的文件数**这一列是[有界扫描](#一次快照覆盖什么):一次快照覆盖的是"这一轮有可能改到"的范围,而不是根目录下的一切。**之后每次**这一列是内容寻址 —— 插件自己那个仓库的第二次捕获一个文件都没有重新哈希,84 个全部复用,所以对一个没变过的文件捕获十次,只存一份。

在 dsh 之前是一个 OpenAI Codex 的 fork,这个引擎是在那里写出来的,设计也是在那里先落的地。但它不是第二个可以拿来读的例子:那个 fork 是祖先,不是使用者,它自带一份 inline 的拷贝,两边的修复不会在任何方向上互相流动。

## 你要的是哪一个

| | |
|---|---|
| **[`filesnap-cli`](crates/filesnap-cli)** | `filesnap` 这条命令。一个人、一个 shell 脚本,或者任何语言写的程序,装的是它。 |
| **[`filesnap`](crates/filesnap)** | 引擎,作为库。Rust 程序嵌的是它。 |

```console
$ cargo install filesnap-cli    # 装出 `filesnap` 这个二进制
$ npm install -g filesnap       # 同一条命令,预编译好的
```

crate 名带 `-cli` 后缀而命令本身不带,就像 `ripgrep` 产出 `rg` 一样。npm 包是一个启动器:你所在平台的构建 —— Linux、macOS、Windows,x64 与 arm64 —— 作为可选依赖装进来,所以安装时下载的是六个里的一个,而不是六个。

两者各自还有更细的 README:[命令面](crates/filesnap-cli/README.md) —— 九条命令、JSON Lines 契约和退出码 —— 以及[引擎的设计](crates/filesnap/README.md)。

## 它不会做的事

- **碰你的版本控制。** git 只读,从不写。
- **删一个它从没观察过的文件。** 只有当它正在还原到的那次捕获**找过**这个路径、而且没找到时,还原才会删除它。墓碑记录是还原唯一的删除许可。
- **因为一个坏文件毁掉整次还原。** 写不进去的文件会被点名,其余的照样落地,退出码也会说出来 —— 是 `1`,不是 `0`。
- **快照你排除掉的东西。** `.filesnapignore` 是对称的:被忽略的路径不会被存、不会被还原,也不会被一次还原删掉。
- **随着你的目录一起长大。** 被追踪的集合是三个有界分区的并集,没有一个随目录规模伸缩 —— 见[一次快照覆盖什么](#一次快照覆盖什么)。

## 一次快照覆盖什么

三个分区,取并集。每一个回答的问题都不同,而且每一个的界限都不是目录树的大小 —— 这正是朴素的全子树遍历所缺的性质,也是它被弃用的原因。

1. **git 追踪的** —— 项目自己认定的那些文件,直接读索引文件本身:索引是一个文件,`git ls-files` 是一个进程,而这段代码每一轮开头都要跑。它的界限是项目本身,而不是项目里被构建出来的东西,因为构建产物恰恰就是不会被提交的那些。这里不会有文件因为太大而被丢掉:一个项目提交了什么,那就是这个项目自己的内容,多大都算。
2. **编辑碰过的** —— 宿主在写入之前的那一刻声明的路径,那是旧字节还存在的最后一刻。它的界限是 agent 做过什么,而不是磁盘上有什么:每一条都是有人特意改过的文件,这里没有任何东西随目录规模增长。一个路径之后还被看多久由你定 —— `--declared-window`,默认 99 轮,也可以是 `unlimited`。
3. **最近改过的** —— 残差:shell 命令或者用户自己的编辑器在前两个分区之外改动的东西。这是唯一一个可能被大目录淹掉的分区,所以也是唯一一个带硬预算的 —— 每个 root 100 个文件、超过 16 MB 的不要,再加一份"高频变动目录"的跳过清单(`node_modules`、`target`、`dist`、`vendor`,等等)。

**没有仓库时,第一个分区就是空的**,另外两个把工作区扛起来。那是一等情形,不是降级情形。

**顺序是吃重的。** 最近改过的那个分区跑在最后,并且被告知前两个已经拿到了什么。在本仓库上实测,加这条排除之前,它 100 个槽位里有 97 个给了 git 索引早就提供过的文件 —— 这件事一直看不见,直到某一次被改动的追踪文件足够多(一次 `cargo fmt`、一次切分支、一次代码生成),把真正未被追踪的那些整个挤出列表为止。

**遍历永远不可能是唯一的分区,再便宜也不行。** 只有清单型的分区才能提出一个"其实不在那里"的路径,而一个被提出却没找到的路径,正是还原被允许删除任何东西之前所需要的墓碑记录。建立在遍历之上的引擎能把删掉的文件放回来,却永远删不掉一个被创建出来的文件。

**最近改过的这次遍历会跳过隐藏条目**,所以 `.env` 和凭据文件不会从这条路进来 —— 但项目**提交了**的隐藏文件依然会经由索引进来,而恰好是隐藏文件的工作产物也依然会从编辑 seam 进来。上面那条 `.filesnapignore` 才是一次把所有方向都关上的规则。

在另一个真实工作中的仓库上,一次子树遍历数出 70,609 个文件、116 GB;git 追踪的分区加上残差分区一共是 6,096 个文件、59 MB。在你自己的仓库上复现:

```text
cargo run -p filesnap --example scan_bench -- /path/to/repo
```

完整覆盖不是一个承诺,而这个缺口是被写出来的,不是被暗示的:一个由 shell 命令创建在 `target/` 里、创建在某个点开头的目录里、超过体积上限,或者在繁忙的一轮里落在最近改过的预算之外的文件,只有当它同时走过编辑 seam 时才被覆盖。`filesnap status` 会点名扫描看到过却存不下来的每一个文件,以及为什么 —— 太大、读不了,或者不是普通文件。它点不出来的两个界限,是它根本没有下去过的高频变动目录,以及预算之外的长尾;而覆盖这两者,正是编辑 seam 的用处。

## 它把东西存在哪

在你所在平台的数据目录下(Unix 上是 `$XDG_DATA_HOME` 或 `~/.local/share`,Windows 上是 `%LOCALAPPDATA%`),从不放进你的项目里。`--data-dir` 可以覆盖它。

格式版本是路径的一部分 —— `filesnap/v2/` —— 所以升级既不迁移也不猜。一个构建遇到它读不懂的存储时会拒绝,而不是误读;更旧格式的存储会被原样留着,而不是被改写。

## 从源码构建

Rust 1.89 或更新(由会话锁用到的 `std::fs::TryLockError` 决定),edition 2024。

```console
$ cargo test --workspace --features filesnap/test-support
$ cargo clippy --workspace --all-targets --features filesnap/test-support
```

CI 在 Linux、macOS 和 Windows 上跑整套测试,外加一次声明的 MSRV 构建。与平台相关的行为是被测出来的,不是被预测的:仅限 Windows 的测试在 [`crates/filesnap/tests/windows.rs`](crates/filesnap/tests/windows.rs),对应的 Unix 那半在 [`permissions.rs`](crates/filesnap/tests/permissions.rs)。

## 设计记录

这个项目要守的规则在 [`.specify/memory/constitution.md`](.specify/memory/constitution.md),规则之下那些编号的决定在 [`decisions.md`](.specify/memory/decisions.md),而它目前还没做到的地方在 [`compliance.md`](.specify/memory/compliance.md)。源码里的注释引用决定编号,而不是把理由重述一遍。

## 许可

Apache-2.0。见 [LICENSE](LICENSE) 与 [NOTICE](NOTICE) —— 本项目起源于 [OpenAI Codex](https://github.com/openai/codex) 的一个 fork,并以同一许可发布。
