# 系统打开入口的接管与恢复方案（Issue #16 研究＋原型）

日期：2026-11-08。原型代码：[`src/sysentry/`](../../src/sysentry)（参数解析 `args.rs`、接管计划与备份恢复 `takeover.rs`、注册表实现与测试根 `winreg.rs`），尚未接入主程序。

本文给 #17（接管文件夹打开并复用标签）和 #18（接管文件定位）提供入口清单、适配方式和未覆盖项。**本机没有做任何真实接管**：没有修改 HKCR、HKLM、`HKCU\Software\Classes` 下的任何键，没有替换 explorer，没有安装外壳扩展。所以下文所有“接管后会怎样”的结论都只到“机制推断”或“公开资料”为止，真实效果要在受控环境验证（见 [验收清单](../checklist/checklist.md) 末尾）。未证实的入口不算兼容。

## 1. 结论

1. **可行且可恢复的主路径**：在 `HKCU\Software\Classes` 下给 `Directory`、`Drive` 增加本应用自己的动词 `slintfm.open`，并把这两个类的默认动词切换过去。只写当前用户、不需要管理员，恢复就是把这几个值按备份还原。Directory Opus（“仅文件系统文件夹”模式）、XYplorer、Total Commander 社区做法都属于这一类。
2. **不碰 `Folder`**：`Folder` 覆盖所有外壳文件夹，包括“此电脑”、回收站、控制面板、网络等虚拟位置，本应用打不开这些位置。Files 当前的公开脚本改写的是 `Folder\shell\open`／`explore`，本方案明确不采用。
3. **不改写系统动词 `open`／`explore`**：只新增动词并切换默认值。这样本应用要把请求交回 Windows 时，显式调用 `open` 动词仍然到达资源管理器，不会循环回到本应用。
4. **Win+E 可单独接管**：覆盖 `HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command`，同样只写当前用户（Files 公开做法）。
5. **“在文件夹中显示”分两类，都不在默认动词接管的可靠覆盖范围内**：
   - 走 `SHOpenFolderAndSelectItems` 的程序（Chrome／Edge 下载、VS Code、Firefox、新版 QQ，均有源码或静态证据）：系统先在已注册的外壳窗口里找目标文件夹；要让本应用选中文件，需要把窗口注册进 `IShellWindows` 并实现选中接口（Explorer++ 的公开做法）。应用未运行时会怎样（是否经默认动词启动、选中是否丢失）未证实。
   - 直接启动 `explorer.exe /select,<文件>` 的程序（微信 4.x 有静态字符串证据）：只有替换或拦截 explorer.exe 才能接管，本方案不做。
6. **任务栏、开始菜单的“文件资源管理器”图标、explorer.exe 直接启动、文件选择对话框内部导航**都不在接管范围，与 Issue #1“桌面、任务栏、开始菜单由 Windows 提供”一致。
7. **最大风险是“程序不见了但关联还在”**：默认动词指向的 exe 被移动或删除后，桌面和资源管理器里双击任何文件夹都会报错（XYplorer 论坛有用户报告卸载后“所有文件夹都打不开”）。分发包是 zip、没有卸载程序，必须有应用内停用、启动自检和独立恢复手段。

## 2. 建议首批范围

| 编号 | 入口 | 首批 | 说明 |
| --- | --- | --- | --- |
| E1 | 资源管理器窗口、桌面上双击文件夹 | 是 | `Directory` 默认动词 |
| E2 | “此电脑”或资源管理器里双击驱动器 | 是 | `Drive` 默认动词 |
| E3 | 右键菜单的默认项（粗体） | 是 | E1/E2 的副作用：粗体项变成“用 Slint 文件管理器打开”；“打开”“在新窗口中打开”仍是资源管理器 |
| E4 | 程序以默认动词打开文件夹（`ShellExecuteEx` 不指定动词） | 是（随 E1/E2） | 哪些程序这样调用需逐个实测 |
| E5 | Win+E | 待用户确认 | 机制独立、风险低，但 Win11 未实测 |
| E6 | `SHOpenFolderAndSelectItems`（浏览器下载、VS Code 等“在文件夹中显示”） | 进入 #18 原型，不承诺 | 需要 `IShellWindows` 注册；冷启动行为未证实。#18 结果：已打开的文件夹产品实测可用，冷启动仍未证实（第 11 节） |
| E7 | 用户可配置命令的程序（Everything 等） | 是（用户自配） | 用户把“打开路径”命令设为本应用，参数用 explorer 风格，由 `args` 解析 |
| — | `Folder` 虚拟位置、显式 `open`／`explore`、explorer.exe 直接启动、`/select`、任务栏和开始菜单图标、文件对话框 | 否 | 见第 4 节 |

## 3. 证据等级与测试环境

| 等级 | 含义 |
| --- | --- |
| 只读实测 | 在本机只读查询或静态扫描得到，没有写入任何关联 |
| 演练实测 | 在测试专用根 `HKCU\Software\slint-file-manager-test\<随机>` 写入并恢复，测试后整棵删除 |
| 源码证实 | 调用方源码中能看到具体调用 |
| 静态证据 | 二进制中出现相关字符串或导入，不等于运行时一定这样调用 |
| 公开资料 | 厂商文档或社区帖子，标注来源 |
| 未证实 | 没有实测也没有可靠来源 |

环境：Windows 10 专业工作站版 22H2，10.0.19045.6456，x64（`[Environment]::OSVersion`、注册表 `UBR`）。没有 Windows 11 环境，Win11 一律“未实测”。

### 3.1 本机注册基线（只读实测）

| 位置（HKCR 合并视图） | 本机内容 |
| --- | --- |
| `Folder\shell` 默认值 | 未设置（默认动词即 `open`） |
| `Folder\shell\open\command` | `(默认)=%SystemRoot%\Explorer.exe`，`DelegateExecute={11dbb47c-a525-400b-9e80-a54615a090c0}`（`CLSID_ExecuteFolder`） |
| `Folder\shell\explore`、`opennewwindow`、`opennewprocess`、`opennewtab` | 都只有 `DelegateExecute={11dbb47c-...}`；Win10 19045 上已经有 `opennewtab`（`OnlyInTabWindow`） |
| `Directory\shell` 默认值 | `none`（来自 HKLM；不存在名为 none 的动词，于是落到 `Folder` 的 `open`） |
| `Drive\shell` 默认值 | `none`（来自 HKLM） |
| `CLSID\{52205fd8-...}\shell\open`、`OpenNewWindow` 的 `command` | `DelegateExecute={52205fd8-...}`（Win+E 的启动对象自身） |
| `HKCU\Software\Classes\Directory\shell`、`Drive\shell` | **键已存在**（Zed 写入了自己的 `Zed` 动词），但没有默认值 |
| `HKCU\Software\Classes\Folder`、`HKCU\...\CLSID\{52205fd8-...}` | 不存在 |

由此得到两条对实现有直接影响的观察：

- **HKCR 合并按值回落**：`HKCU\...\Drive\shell` 键存在但没有默认值时，`HKCR\Drive\shell` 的默认值仍然是 HKLM 的 `none`。因此覆盖 Win+E 的 `command` 键时，必须在 HKCU 显式写一个空的 `DelegateExecute`，否则 HKLM 的 `DelegateExecute` 仍可能生效（Files 的脚本也这样写；“空值确实让外壳改用命令行”这一点本机未实测）。
- **备份必须区分三种原状**：“键不存在”“键存在但没有这个值”“值存在”。本机的 `Directory\shell` 属于第二种：恢复时只能删除默认值，不能删除键，否则会连带删掉 Zed 的动词。演练用例按这个真实形态布置（见第 6 节）。

## 4. 入口清单

列说明：**机制**是系统内部走的调用路径；**适配方式**是本应用怎么接；**恢复方式**对应第 6 节的备份恢复；**首批**见第 2 节。

| 入口与触发场景 | 机制 | 适配方式 | 恢复方式 | Win10 实测状态 | Win11 状态 | 建议首批 | 风险 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| E1 资源管理器窗口内、桌面上双击文件夹 | 外壳视图（DefView）调用项目的默认命令；默认动词是浏览类动词时在原窗口导航，否则执行该动词（Opus、XYplorer 等依赖这一点） | `HKCU\...\Directory\shell` 默认值 = `slintfm.open`；`slintfm.open\command` = `"<exe>" --entry "%1"` | 按备份还原默认值、删除自建动词键 | 未实测（需 VM）；只读核对了基线 | 未实测（据 Opus、TC 社区帖，Win11 上同类做法可用） | 是 | 资源管理器里双击文件夹也会跳到本应用（Opus 同样如此）；exe 失效时所有文件夹打不开 |
| E2 “此电脑”或资源管理器里双击驱动器 | 同上，类为 `Drive` | `HKCU\...\Drive\shell` 默认值 = `slintfm.open`，命令同上 | 同上 | 未实测（需 VM） | 未实测 | 是 | 同上；`%1` 为 `C:\` 时 MSVC 参数规则会吃掉反斜杠，已用原始命令行解析规避 |
| E3 右键菜单默认项 | 粗体项就是默认动词 | 随 E1/E2 | 随 E1/E2 | 未实测 | Win11 新菜单（IExplorerCommand）里的显示位置未实测；双击行为不受菜单样式影响 | 是 | 用户仍可选“打开”回到资源管理器，属于预期的逃生口 |
| E4 程序以默认动词打开文件夹（`ShellExecuteEx`，`lpVerb=NULL`） | 解析默认动词 | 随 E1/E2 | 随 E1/E2 | 未实测；具体程序待逐个测 | 未实测 | 是 | Opus 论坛建议程序作者传 NULL 动词；传了 `open` 的程序不会被接管 |
| 程序显式用 `open`／`explore` 动词打开文件夹（Chromium 的 `ShowItemInFolder` 回退分支即 `ShellExecute(..., "open", 父目录)`，源码证实） | 按名字找动词，落到 `Folder\shell\open`（`CLSID_ExecuteFolder`） | 不接管。要接管只能改写 `open`，会让交回 Windows 的路径也回到本应用，形成循环 | — | 不涉及 | 不涉及 | 否 | `CLSID_ExecuteFolder` 内部是否再看默认动词未证实，列入 VM 检查 |
| E5 Win+E | 外壳热键 → `CLSID\{52205fd8-...}\shell\opennewwindow` 的委托执行对象 | `HKCU\...\CLSID\{52205fd8-...}\shell\opennewwindow\command`：`(默认)="<exe>" --entry`，`DelegateExecute=""`（Files 公开做法） | 删除自建的整条键链（备份时不存在） | 未实测（需 VM）；只读核对了 HKLM 原状 | 未实测；XYplorer 论坛有人报告 Win11 上手改后重启被还原，结论互相矛盾 | 待用户确认 | 开始菜单“文件资源管理器”图标走 explorer.exe，不受影响，两条入口行为会不一致 |
| E6 `SHOpenFolderAndSelectItems`：Chrome／Edge 下载“在文件夹中显示”、VS Code“在文件资源管理器中显示”、Firefox“打开所在文件夹”、新版 QQ | 先在 `IShellWindows` 已注册窗口中找显示目标文件夹的窗口并选中；找不到时新开窗口（具体走哪个动词未证实） | #18 原型：把本应用窗口按当前文件夹 PIDL 注册进 `IShellWindows`，实现 `IServiceProvider`→`IShellBrowser`→`IShellView`／`IFolderView` 的选中链（Explorer++ 提交 67c3687、RegShellWindow 的做法）；冷启动依赖 E1/E2 | 运行时注册，进程退出即注销，无持久配置 | 调用方：Chromium、Electron、VS Code 源码证实，Firefox 源码证实，QQNT.dll 静态证据；接管效果未实测 | 未实测 | 进入 #18 原型，不承诺 | 实现量大、接口多；Opus 员工也说“更难拦截”；冷启动时选中可能丢失 |
| explorer.exe 直接启动：运行框或命令行 `explorer <路径>`、`explorer /e,`、`/root,` | explorer.exe 自己解析参数 | 不接管。是否随 `Directory` 默认动词转发未证实（Opus 员工称“有时可以但不推荐”） | — | 未实测 | 未实测 | 否 | 若转发会造成用户心智不一致，VM 中记录实际行为 |
| `explorer /select,<文件>`：微信 4.2 的“在文件夹中显示”（`Weixin.dll` 含 `/select,` 与 `explorer.exe` 字符串，不含 `SHOpenFolderAndSelectItems`，静态证据）、其他老程序 | explorer.exe 解析 `/select` | 不接管；只有替换或拦截 explorer.exe 才能接管（见“不采用的方式”） | — | 未实测 | 未实测 | 否 | 列为未覆盖，回退到资源管理器 |
| 任务栏、开始菜单固定的“文件资源管理器” | 快捷方式直接启动 explorer.exe | 不接管（Issue #1：任务栏、开始菜单由 Windows 提供） | — | 不涉及 | 不涉及 | 否 | — |
| 桌面“此电脑”“回收站”“网络”“控制面板”等虚拟位置 | `Folder` 或各自 CLSID 的动词 | 不接管；首批计划完全不写 `Folder` | — | 不涉及（计划层面已用单元测试保证不写 `Folder`） | 不涉及 | 否 | 若改写 `Folder` 会打不开控制面板等系统页面（Opus 文档特别说明控制面板始终交给资源管理器） |
| 开始菜单或跳转列表里的“文档”“图片”“下载”等已知文件夹 | 已知文件夹的外壳项；是否按 `Directory` 类解析未证实 | 若随 E1 被接管，按普通目录处理；`shell:` 名称由 `args` 归为“需运行时解析” | 随 E1 | 未实测 | 未实测 | 随 E1，不单独承诺 | — |
| 库（`.library-ms`）、压缩包（`CompressedFolder`）、搜索（`search-ms:`）、`shell:AppsFolder` | 各自的类与动词 | 不接管 | — | 不涉及 | 不涉及 | 否 | 压缩包按 ADR 0005 交给 NanaZip |
| 文件选择对话框中双击文件夹 | 对话框内部导航，不走默认动词（Opus 文档：打开／保存对话框不被替换） | 不涉及（第三阶段另行处理） | — | 未实测 | 未实测 | 否 | VM 中确认对话框行为不变 |
| E7 Everything 等允许自定义“打开路径”命令的程序 | 程序按用户配置的命令行启动 | 用户把命令设为 `"<exe>" /select,"%1"` 或 `"<exe>" "%1"`；`args` 按 explorer 规则解析 | 用户改回原设置 | 解析器单元测试覆盖；与具体程序联调未实测 | 同左 | 是（用户自配） | 各程序的占位符语法不同，需按程序写说明 |
| U 盘自动播放“打开文件夹以查看文件”、OneDrive 托盘“打开文件夹”、通知中心 | 未证实 | — | — | 未证实 | 未证实 | 否（VM 中记录） | — |

### 4.1 不采用的方式

| 方式 | 不采用的原因 |
| --- | --- |
| 改写 `Folder` 的默认动词或 `Folder\shell\open`（Files 当前公开脚本） | 覆盖虚拟位置；改写 `open` 还会让“交回 Windows”的路径循环回本应用 |
| `HKLM` 写入、或“所有用户”范围（XYplorer 提供此选项） | 需要管理员，影响其他账户，超出 Issue #1 的需求 |
| explorer.exe 的映像劫持（IFEO `Debugger`） | 需要管理员；会劫持桌面外壳进程本身，风险不可接受 |
| 替换 Winlogon `Shell`、替换 explorer.exe | 违背“桌面、任务栏由 Windows 提供”，且本任务明确禁止 |
| 进程注入或挂钩 `SHOpenFolderAndSelectItems` | 需要往别的程序里注入代码，安全软件会拦截，稳定性不可控 |
| 外壳扩展（复制钩子、命名空间扩展等） | 会加载进资源管理器进程，崩溃会拖垮桌面；本任务明确禁止安装 |

## 5. 适配方式（给 #17/#18）

### 5.1 注册计划（`takeover::plan`）

写入顺序：先建动词和命令，最后切换默认动词；恢复时倒序。路径都相对 `HKCU\Software\Classes`。

| 范围 | 键 | 值名 | 数据 |
| --- | --- | --- | --- |
| `Scope::FileSystemFolders` | `Directory\shell\slintfm.open` | (默认) | `用 Slint 文件管理器打开` |
| | `Directory\shell\slintfm.open\command` | (默认) | `"<exe>" --entry "%1"` |
| | `Directory\shell` | (默认) | `slintfm.open` |
| | `Drive\shell\...` | | 与 Directory 相同的三项 |
| `Scope::WinE` | `CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command` | `DelegateExecute` | 空串 |
| | 同上 | (默认) | `"<exe>" --entry` |

应用前用 `takeover::preflight` 列出“将被覆盖的已有值”，例如别的文件管理器已经把 `Directory\shell` 默认值设成了自己的动词。这种情况要先让用户确认，不能静默覆盖。

### 5.2 命令行与参数解析（`args`）

- 读取原始命令行（`sysentry::raw_command_line()` → `args::strip_program` → `args::parse`），**不用** `std::env::args()`。MSVC 规则会把 `"C:\"` 解析成 `C:"`；explorer 风格参数又以逗号分隔，两者都只能从原始字符串正确解析。
- 支持：`--entry`（本应用动词）、裸路径、`/select,`、`/root,`、`/e`、`/n`、`/separate`；带引号路径、引号内的逗号、中文与空格；`C:` → `C:\`；`/` → `\`；`.`、`..` 按字面消去；相对路径按工作目录解析；`\\?\` 与 `\\?\UNC\` 前缀；UNC；`file:` URL（百分号转义按 UTF-8 解码）；`shell:<名称>`、`shell:::{GUID}`、`::{GUID}[\剩余]`。
- 无法处理的部分记入 `unsupported`：`/idlist,:句柄:进程`（共享内存 PIDL，拿不到路径）、`/factory,{...}`、`-Embedding`（COM 激活）、未知开关、多余的位置参数。
- `EntryRequest::disposition()` 给出处理方式：
  - `OpenFolder`：目录、驱动器根、回收站（`shell:RecycleBinFolder`、`::{645FF040-...}`）。
  - `OpenAndSelect`：`/select` 的父目录加目标项；选中驱动器根时直接打开该盘。
  - `OpenDefault`：Win+E、裸 `explorer`。
  - `ResolveShell`：`shell:Downloads` 等已知文件夹，运行时用外壳解析成文件系统路径，解析不到就交回 Windows。
  - `System`：“此电脑”（应用没有对应位置）、控制面板等虚拟位置、`/idlist` 等不透明参数。
- 参数从不被当作命令执行：形似命令的输入只会成为（通常不存在的）路径或无法解析的目标，有单元测试。
- 解析器不访问磁盘。目标是否存在、是文件还是目录由 #17 在运行时判断。注意 `explorer <文件>` 的原生行为是用默认应用打开该文件，#17 需要决定：是交给 `shell::open`，还是按定位处理。

### 5.3 交回 Windows 与防循环

- 交回 Windows 时一律用**显式 `open` 动词**（`ShellExecuteExW`，`lpVerb="open"`，必要时 `SEE_MASK_CLASSNAME` + `lpClass="Folder"` 或 `SEE_MASK_IDLIST`），不用默认动词，也不用不带动词的 `ShellExecute`。本方案从不改写 `open`，所以这条路径不会回到本应用。`CLSID_ExecuteFolder` 内部是否会再读默认动词未证实，列入 VM 检查项。
- 文件系统路径打不开（不存在、无权限）时，在应用内提示，不交回资源管理器。这和标签“位置不可用”的既有规则一致（docs/panes-tabs-session.md）。

### 5.4 与既有标签规则的衔接

收到请求后的标签行为沿用 ADR 0002 与 docs/panes-tabs-session.md 的“外部请求打开目录”：优先激活已有同路径标签（先左后右），否则在当前面板新开标签；`OpenAndSelect` 还要选中目标项。单实例与跨进程转发（第二个进程把请求交给已运行的主实例后退出）属于 #17 的范围，本次没有实现。`/n`、`/separate` 只作记录，不改变“复用现有窗口”的规则。

## 6. 恢复方案与演练

### 6.1 规则（`takeover` 模块）

1. **写入前备份**（`capture`）：逐值记录原值（含类型和原始字节）；记录哪些键（包括祖先键）原来不存在。
2. **先持久化再写入**（`apply` 的 `persist` 回调）：建议保存为 `%LOCALAPPDATA%\slint-file-manager\entry-takeover-backup.json`（`Backup::to_json`，带 `version`）。持久化失败就不写注册表。
3. **写入中途失败立即回滚**，回滚结果随错误一起返回，不标记“已启用”。
4. **只回滚仍是自己写入的值**（`restore`）：当前值等于本应用写入的值才还原；等于原值视为已恢复；两者都不是就记为冲突，不覆盖、不删除。
5. **只删除自己建的空键**：备份时不存在、且恢复后没有任何值和子键的键，由深到浅删除；里面有别人后来写的内容就保留并报告。
6. **逐值复核**：恢复后再读一遍，与备份比对，报告冲突、残留键和不一致；`RestoreReport::is_clean()` 为真才算恢复成功。
7. **状态查询**（`status`）：返回“已接管／未接管／部分生效（列出不一致项）”，供 #17 的设置界面显示，不显示虚假的“已启用”。

### 6.2 演练结果（演练实测，本机 Win10）

`cargo test --lib sysentry`（已包含在 `scripts/verify.ps1`）：

- `winreg::tests::rehearsal_in_real_test_root`：在 `HKCU\Software\slint-file-manager-test\<pid>-<纳秒>` 下按本机真实形态布置初始状态：`Directory\shell` 键存在、无默认值、旁边有别的程序的动词（同 Zed）；`Drive\shell` 默认值为 `none`，另有 REG_EXPAND_SZ 和 REG_DWORD 值；`CLSID\{52205fd8-...}` 完全不存在。随后依次执行快照、应用前检查（确认指出 `Drive\shell` 默认值将被覆盖）、应用、确认“已接管”、备份 JSON 往返（模拟重启）、恢复，再与快照**逐键逐值比对一致**，并确认状态为“未接管”。最后整棵删除测试根并确认已不存在。
- `winreg::tests::real_test_root_conflict_and_types`：原值为 REG_EXPAND_SZ 时，恢复后类型和字节一致；应用后另一程序改写 `Drive\shell` 默认值，恢复时报告冲突，保留对方的值，删除自己的动词键。
- 内存实现上的同构用例：原来什么都没有、写入第 5 项失败后的回滚、持久化失败不写入、重复恢复幂等、外来子键阻止删除自建键、备份 JSON 拒绝坏输入和错版本。
- `winreg::tests::refuses_to_delete_outside_test_prefix`：整棵删除只允许发生在测试前缀之下，`Software\Classes` 等路径会被拒绝。

测试结束后核对过：`HKCU\Software\slint-file-manager-test` 已不存在；`HKCU\Software\Classes\Directory\shell`、`Drive\shell` 与测试前完全一致（仍只有 Zed 的动词，没有默认值）；`HKCU\Software\Classes\CLSID\{52205fd8-...}` 仍不存在。

### 6.3 恢复不了或不该自动恢复的情况（#17 必须处理）

| 情况 | 现象 | 建议处理 |
| --- | --- | --- |
| 程序被移动、删除，或 zip 解压到别处后旧目录被删 | 默认动词指向不存在的 exe，双击任何文件夹都报错（XYplorer 论坛有卸载后“所有文件夹都打不开”的报告） | 应用每次启动时用 `status` 和命令路径自检，路径不符就提示“修复或停用”；附带不依赖本程序的恢复办法：在 `HKCU\Software\Classes\Directory\shell`、`Drive\shell` 删除默认值、删除 `slintfm.open` 子键，删除 `HKCU\Software\Classes\CLSID\{52205fd8-...}`（仅当它是本应用建的）。分发包里应附说明或脚本 |
| 备份文件丢失或损坏 | 无法得知原值 | 不猜测原值；只能删除本应用写入的值，这在原值本来就不存在时等价于恢复（本机即如此），否则要提示用户 |
| 别的文件管理器在本应用之后接管 | 恢复时出现冲突 | 不覆盖对方，提示用户 |
| 别的文件管理器在本应用之前已接管 | `preflight` 列出将被覆盖的值 | 让用户确认；恢复时还原为对方的值 |

## 7. Windows 10 与 Windows 11 的差异

本机只有 Windows 10。以下 Win11 内容只依据公开资料，全部**未实测**：

- Win11 22H2 起资源管理器有标签。Win10 19045 的注册表里也已经有 `opennewtab` 动词，本方案不改它。
- Win11 的新右键菜单只显示 `IExplorerCommand` 等新式命令，注册表动词出现在“显示更多选项”里。默认动词决定的双击行为与菜单样式无关，但需要实测。
- Win11 对文件扩展名和协议的默认应用有 UserChoice 哈希保护；目前没有资料表明 `Directory`、`Drive` 的 `shell` 默认动词受这一保护，需要实测。
- XYplorer 论坛里有 Win11 用户报告“取消默认文件管理器无效”，也有人报告手改 Win+E 后重启被还原；Total Commander 论坛有用户报告 Win11 24H2 上 `Directory`／`Drive` 做法可用。说法互相矛盾，只能靠 VM 验证。

## 8. 需要用户确认的问题

见 Issue #16 评论，摘要如下：

1. 首批接管范围：A. 只接管文件系统文件夹和驱动器（推荐）；B. A 加 Win+E；C. 再加虚拟位置（不推荐）。
2. 在资源管理器窗口里双击文件夹也会跳到本应用，这是 `Directory` 默认动词接管的必然结果：A. 接受；B. 不接受（目前没有公开且可靠的“只接管外部请求”的做法，需要另外研究）。
3. Win+E 的行为：A. 激活现有窗口，没有窗口时按会话恢复；B. 在当前面板新开主目录标签；C. 不接管 Win+E。
4. “在文件夹中显示”（`SHOpenFolderAndSelectItems`）：A. #18 做 `IShellWindows` 注册原型，未运行时的冷启动行为按 VM 结果再定；B. 首批不做，退回资源管理器。
5. 直接调用 `explorer.exe /select` 的程序（如微信）：A. 不接管，明确列为未覆盖（推荐）；B. 研究注入或映像劫持（不推荐，需要管理员，风险高）。
6. zip 分发、没有卸载程序时的保护：A. 应用内停用、启动自检和独立恢复说明（推荐）；B. 增加安装程序（扩大范围）。
7. 真实接管的验收环境：A. 提供 Win10 与 Win11 虚拟机（可做快照）；B. 在本机新建一个专用测试账户（方案只写 HKCU，可隔离）；C. 只做 Win10 虚拟机，Win11 继续标“未实测”。

## 9. 未覆盖与仍未知事项

- 真实接管下的全部行为（E1–E6）：本机不做，列入验收清单，在受控环境验证。
- `SHOpenFolderAndSelectItems` 在找不到已注册窗口时走哪个动词、选中是否保留（#18 原型后仍未知，见第 11 节）。
- `explorer.exe <路径>`、`explorer /select,` 是否随 `Directory` 默认动词转发（#18 已实测：未接管时即使文件夹已登记为外壳窗口也新开资源管理器；接管后仍未知）。
- `CLSID_ExecuteFolder`（`open` 动词的委托对象）是否会再读默认动词，即交回 Windows 时是否可能循环。
- Win+E 覆盖中空 `DelegateExecute` 的实际效果。
- 微信、QQ 的调用方式只有静态字符串证据；浏览器下载以外的常见程序（IDM、迅雷、qBittorrent、Office“打开文件位置”等）未调查。
- Windows 11 全部未实测。

## 10. 来源

- Files：[SetFilesAsDefault.reg](https://github.com/files-community/Files/blob/main/src/Files.App/Assets/FilesOpenDialog/SetFilesAsDefault.reg)、[UnsetFilesAsDefault.reg](https://github.com/files-community/Files/blob/main/src/Files.App/Assets/FilesOpenDialog/UnsetFilesAsDefault.reg)（当前改写 `Folder\shell\open`／`explore` 与 Win+E 的 CLSID 键；早期版本用 `Directory\shell\openinfiles`）、[PR #7408](https://github.com/files-community/Files/pull/7408)、[Issue #16251](https://github.com/files-community/Files/issues/16251)
- Directory Opus：[Explorer Replacement（Opus 11 手册）](https://www.gpsoft.com.au/help/opus11/Documents/Prefs/Explorer_Replacement.htm)（“仅文件系统文件夹／所有文件夹／排除列表”；控制面板始终交给资源管理器；不替换打开／保存对话框）、[论坛：Replace Explorer in all calls?](https://resource.dopus.com/t/replace-explorer-in-all-calls/14843)（建议程序用 NULL 动词；`SHOpenFolderAndSelectItems` 更难拦截；直接启动 explorer.exe 不推荐）
- XYplorer 论坛：[Setting Default File Manager using Registry](https://www.xyplorer.com/xyfc/viewtopic.php?p=190847)、[卸载后所有文件夹打不开](https://www.xyplorer.com/xyfc/viewtopic.php?p=152491)、[Windows 11 无法取消默认文件管理器](https://www.xyplorer.com/xyfc/viewtopic.php?p=191883)
- Total Commander：[Ghisler Wiki：Explorer integration](https://www.ghisler.ch/wiki/index.php/Explorer_integration)、[论坛：Set TC as default file manager?](https://ghisler.ch/board/viewtopic.php?p=389527)
- `SHOpenFolderAndSelectItems` 与窗口注册：[Explorer++ 提交 67c3687 “Register as a shell window”](https://github.com/derceg/explorerplusplus/commit/67c3687f3012330d802d70028f36968788d80aff)、[Stack Overflow 64286041 存档](https://kaweah.freedombox.rocks/kiwix/content/stackoverflow.com_en_all_2023-11/questions/64286041/show-in-folder-shopenfolderandselectitems-for-explorer-replacement-program)、RegShellWindow（github.com/fafalone/RegShellWindow）
- 调用方源码：[Chromium `chrome/browser/platform_util_win.cc`](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/chrome/browser/platform_util_win.cc)（`ShowItemInFolder` → `SHOpenFolderAndSelectItems`，`ERROR_FILE_NOT_FOUND` 时 `ShellExecute("open", 父目录)`）、[Electron `shell/common/platform_util_win.cc`](https://github.com/electron/electron/blob/main/shell/common/platform_util_win.cc)（同上）、VS Code `src/vs/platform/native/electron-main/nativeHostMainService.ts`（`showItemInFolder` → Electron `shell.showItemInFolder`）、[Firefox `xpcom/io/nsLocalFileWin.cpp`](https://searchfox.org/mozilla-central/source/xpcom/io/nsLocalFileWin.cpp)（`Reveal`）
- 微软文档：SHOpenFolderAndSelectItems、IShellWindows、“Merged View of HKEY_CLASSES_ROOT”、“Creating Context Menu Handlers / Verbs”、Windows Explorer 命令行开关（`/n`、`/e`、`/root`、`/select`），均在 learn.microsoft.com
- 本机静态扫描：`C:\Program Files\Tencent\Weixin\4.2.0.9\Weixin.dll`（含 `/select,`、`explorer.exe`，无 `SHOpenFolderAndSelectItems`）、`C:\Program Files\Tencent\QQNT\versions\9.9.37-54395\QQNT.dll`（含 `SHOpenFolderAndSelectItems`）。只是字符串存在，不代表运行时调用路径

## 11. #18 原型与集成结果（2026-11-08）

范围按 #16 的用户决定：只做 `SHOpenFolderAndSelectItems` 的 `IShellWindows` 原型（不承诺成功）；`explorer /select,` 列为未覆盖；虚拟位置不接管；与 #17 共用启用与恢复。新增两个证据等级：

| 等级 | 含义 |
| --- | --- |
| 原型实测 | `reveal-probe host`（独立的 Win32 宿主窗口，把指定文件夹登记进 `IShellWindows`）＋ `reveal-probe call`（按 Chromium 源码的参数调用 `SHOpenFolderAndSelectItems`：父目录 PIDL + 文件 PIDL），只用 `%TEMP%` 下的临时目录，不写注册表；登记随宿主进程退出注销 |
| 产品实测 | release 版主程序 + `SFM_TEST_REG_ROOT` 测试根里的“已接管”状态（`reveal-probe test-root take`，与设置面板同一份 `manage::take`），由 `reveal-probe call` 调用；脚本 `scripts/verify-reveal.ps1`，已含在 `scripts/verify.ps1` |

测试根只决定本应用自己是否登记（读到“已接管”才登记），**外壳并不读测试根**，所以“应用未运行”“运行中但文件夹未打开”这两种需要外壳回退去启动本应用的情况，开发机上无法实测，只能在测试账户里人工验收。

### 11.1 外壳的调用链（原型实测）

`SHOpenFolderAndSelectItems` 找到已登记窗口后依次调用：`IShellWindows::FindWindowSW`（只匹配以 `SWC_BROWSER` 登记的窗口）→ `IWebBrowserApp::get_HWND`、`get_Document` → 文档对象的 `IServiceProvider::QueryService(IID_IFolderView, IID_IShellView)` → `IShellView::SelectItem(子项 PIDL, 标志)`。第一个文件的标志是 `0x1d`（选中、取消其他、确保可见、焦点），其余文件是 `0x1`（追加选中）。登记方式：`RegisterPending(线程, 文件夹 PIDL, —, SWC_BROWSER)` 再 `Register(IDispatch, hwnd, SWC_BROWSER)`，两次得到同一个 cookie；导航用 `OnNavigate`，关闭用 `Revoke`。每个标签登记一项（同一 hwnd、不同 cookie），与 Explorer++ 提交 67c3687 相同。实现在 `src/sysentry/shellwin.rs`。

### 11.2 实验结果

| 场景 | 结果 | 等级 |
| --- | --- | --- |
| 文件夹已登记（窗口在前台或最小化） | 调用 29–58 ms 返回 `S_OK`，不开资源管理器；宿主收到 `SelectItem`，路径含中文和空格正确；最小化的窗口被系统还原并切到前台，但不是每次都前置（导航实验里宿主未被前置），所以产品收到选中后自己再还原并前置一次 | 原型实测 |
| 同一窗口登记两个文件夹（两个标签） | 选中请求落到目标文件夹那一项（槽号区分标签） | 原型实测 |
| 两个文件／只传文件 PIDL（`cidl=0`） | 两个文件依次 `0x1d`、`0x1`；只传文件 PIDL 同样选中 | 原型实测 |
| 登记后导航到别的文件夹（`OnNavigate`） | 新文件夹的调用落到该项，旧文件夹回退 | 原型实测 |
| 文件夹未登记 | 资源管理器新开窗口（约 375 ms） | 原型实测 |
| 调用进行中、约 30 ms 后才登记该文件夹 | 新登记的一项也收到 `SelectItem`，资源管理器同时开了窗口（推断：外壳在回退期间仍会查找新登记的窗口；只做过一次） | 原型实测（机制为推断） |
| 回退时用哪个动词启动谁 | 无法在不做真实接管的前提下实测；按上一行推断：若回退经 `Directory` 默认动词启动本应用，本应用尽快登记即可收到选中 | 未证实（推断） |
| 调用方取得 PIDL 后文件被删除 | 外壳仍把请求送到已登记的标签；本应用切到标签并提示“已不存在（可能已被移动或删除）” | 产品实测 |
| 文件本来不存在 | 测试调用器按 Chromium 的做法先解析 PIDL，解析失败就不发出调用，本应用与资源管理器都不参与；真实程序此时怎样处理（例如改为打开父目录）只能人工验收 | 产品实测（调用方部分） |
| 已登记的文件夹被 `ShellExecute(open／默认动词)` 打开 | 不开资源管理器，系统把本窗口还原并切到前台，但**没有任何 COM 调用**，本应用无从得知是哪个标签 | 原型实测 |
| 命令行 `explorer.exe /select,<文件>`（模拟微信的调用方式；微信本身只有静态字符串证据）、`explorer.exe <文件夹>` | 即使该文件夹已登记为外壳窗口，也新开资源管理器窗口 | 原型实测（未做真实接管） |
| `Shell.Application.Windows()`（脚本后期绑定枚举） | 能枚举到本应用的项，但 `HWND`、`LocationURL` 为空（本实现的 `IDispatch` 只提供早期绑定接口） | 原型实测 |
| 已接管 + 主程序运行：已打开的文件夹、转发新开的标签、两个文件 | 在对应标签选中，不开资源管理器；不新建标签；退出时注销 | 产品实测 |
| 已接管 + 主程序运行：未打开的文件夹 | 回退到资源管理器（测试根下外壳不会启动本应用） | 产品实测 |
| 停用接管（同一测试根恢复原设置） | 主程序不登记；已在本应用打开的文件夹也回退到资源管理器；恢复干净 | 产品实测 |
| 真实的 Chrome／Edge／VS Code／Firefox／QQ | 未实测：这些程序的“在文件夹中显示”只能从界面触发（VS Code 没有执行命令的命令行入口，Edge 需要先真实下载文件），开发机上不做 UI 自动化去驱动用户自己的程序；它们走的调用由源码或静态证据确定（第 4 节 E6），测试调用器用的是与 Chromium 相同的参数 | 仅人工验收 |

另外尝试过“只在调用进程里用 `RegOverridePredefKey` 把 HKCR 指到 HKCU 下的测试键”来模拟冷启动：外壳弹出“没有注册类”的阻塞错误框，调用 255 秒后以 `E_ABORT` 返回，宿主没有被启动，结论无效；测试键已删除，该实验方式已从测试调用器中移除。

### 11.3 结论与产品决定

- **可行**：已打开文件夹的“在文件夹中显示”可以可靠地落到本应用对应标签并选中（产品实测）。冷启动和“运行中但文件夹未打开”依赖外壳回退走默认动词，未证实，交给人工验收；最坏情况是只打开文件夹不选中，或回到资源管理器。
- **不单独设开关，与 #17 共用“用本应用打开文件夹”**：登记只在进程存活期间有效，不写持久配置；未接管时冷启动和未打开的文件夹都到不了本应用，单独开启只会得到“有时本应用、有时资源管理器”的不一致体验；登记还会改变 `ShellExecute(open)` 打开已登记文件夹的行为（见上表），没有选择接管的用户不应受影响；多一个开关还要多一套界面与基线。停用接管即刻全部注销。
- **登记范围**：两个面板的全部标签，位置是可访问的文件系统文件夹；回收站与不可用的位置不登记。

### 11.4 仍未知

- 回退时外壳走哪个动词、选中是否保留（冷启动、运行中但文件夹未打开）。
- 已接管时 `explorer.exe /select,` 是否经 `Directory` 默认动词转发到本应用（未接管时已实测回退）。
- 真实 Chrome／Edge／VS Code／Firefox／QQ 的效果；Windows 11 全部未实测。
