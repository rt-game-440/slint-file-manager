# 文件选择对话框测试宿主（Issue #23）

一个只用于测试的小程序：用 Windows 常见的三类方式弹出文件选择对话框，把返回结果完整记录成 JSON。第三阶段（Issue #19 及其后续）可以先在可控环境里验证“选择器替代＋结果回传”，再拿真实软件做最终验收；代理也能自动重复这些场景。它**不代替**用户确认的软件清单，#19 的清单与真实软件验收仍按原规格执行（ADR 0003）。

## 构建

测试宿主是独立 crate `tools/dialog-host`（自带 `[workspace]`、`Cargo.lock` 与 `target`，不进主程序依赖，也不进 `dist/` 分发包）。依赖只有 `windows` 0.62（与主程序同版本，复用全局缓存）和 `serde_json`。

```powershell
rustup target add i686-pc-windows-msvc          # 仅首次；x86 构建需要
cd tools/dialog-host
cargo build --release --locked --jobs 6                              # x64 → target\release\dialog-host.exe
cargo build --release --locked --jobs 6 --target i686-pc-windows-msvc # x86 → target\i686-pc-windows-msvc\release\dialog-host.exe
```

两种位数都必须验证：旧式 `OPENFILENAMEW` 结构在 x86 上是 1 字节对齐（packed），x64 上是默认对齐，替代实现很容易在这里出错，所以不能只用一种位数下结论。

## 命令行

```text
dialog-host run <方式> <场景> [--选项=值 ...]   立即弹框，结果 JSON 输出到标准输出
dialog-host gui [--fixture=目录]                带按钮的窗口（人工操作；无参数时默认）
dialog-host fixture [--root=目录]               重建夹具（默认 %TEMP%\sfm-dialog-host\fixture）
dialog-host list                                输出方式×场景目录 JSON
```

退出码：0 弹框并正常结束（含取消）；1 系统接口报错（JSON 里 `result.error` 有原因）；2 参数错误。标准输出是一行紧凑 JSON（UTF-8）；`--out=文件` 另存一份，脚本读它比读控制台输出更可靠。

### 方式

| 方式 | 系统接口 | 说明 |
| --- | --- | --- |
| `ifd` | `IFileOpenDialog`／`IFileSaveDialog` | 现代通用对话框；选文件夹用 `FOS_PICKFOLDERS`；始终加 `FOS_FORCEFILESYSTEM`，在系统默认选项上叠加 |
| `gofn` | `GetOpenFileNameW`／`GetSaveFileNameW` | 旧式通用对话框，带 `OFN_EXPLORER`；返回缓冲区的布局也被记录 |
| `sbf` | `SHBrowseForFolderW` | 旧式文件夹选择；`--sbf-style=new`（`BIF_NEWDIALOGSTYLE｜BIF_EDITBOX`，默认）或 `legacy`（只有目录树）；初始目录经 `BFFM_INITIALIZED` 回调预选 |

### 场景与选项

| 场景 | `ifd` | `gofn` | `sbf` | 对应的系统设置 |
| --- | --- | --- | --- | --- |
| `open-single` | 是 | 是 | 否 | 打开单个文件 |
| `open-multi` | 是 | 是 | 否 | `FOS_ALLOWMULTISELECT`／`OFN_ALLOWMULTISELECT` |
| `save` | 是 | 是 | 否 | 保存（默认文件名、默认扩展名、覆盖确认） |
| `pick-folder` | 是 | 否 | 是 | 选择文件夹（`gofn` 没有这个模式） |

| 选项 | 含义 |
| --- | --- |
| `--init-dir=目录` | 初始目录（ifd：`SetFolder`；gofn：`lpstrInitialDir`；sbf：回调预选） |
| `--title=文本` | 标题（sbf 是目录树上方的说明文字） |
| `--filter=名称\|模式;模式` | 文件类型过滤器，可重复；例：`--filter=图片\|*.png;*.jpg` |
| `--filter-index=N` | 初始选中的过滤器，从 1 起 |
| `--default-name=文本` | 默认文件名（保存常用） |
| `--default-ext=扩展名` | 默认扩展名，不含点 |
| `--overwrite-prompt=on\|off` | 保存时同名文件的覆盖确认，默认 `on`（ifd：`FOS_OVERWRITEPROMPT`；gofn：`OFN_OVERWRITEPROMPT`） |
| `--sbf-style=new\|legacy` | 仅 sbf |
| `--owner=none\|window` | 所有者窗口：不带（默认）或一个可见的顶层窗口（多数真实软件会传所有者） |
| `--out=文件` | JSON 同时写入该文件 |
| `--log-file=文件`／`--log-dir=目录` | 日志位置，见下 |

**取消不是对话框的配置，而是操作者的动作。** 取消场景由人（带按钮窗口）或自动驱动（`scripts/dialog-baseline.ps1`，点“取消”按钮）产生；宿主只负责记录“取消了”。

例：

```powershell
$fx = "$env:TEMP\sfm-dialog-host\fixture"
.\target\release\dialog-host.exe fixture
.\target\release\dialog-host.exe run ifd open-multi "--init-dir=$fx\docs" "--filter=文本文件|*.txt;*.md" "--filter=所有文件|*.*" "--out=$env:TEMP\r.json"
```

### 输出 JSON

结构固定（键按字母序）：

- `schema`、`tool`、`api`、`scenario`、`bitness`（32／64）。
- `request`：本次的全部请求参数（含默认值）。
- `result`：
  - `return_kind`／`return_code`：`HRESULT`（`0x00000000`；取消是 `0x800704C7`＝`HRESULT_FROM_WIN32(ERROR_CANCELLED)`）、`BOOL`（`TRUE`／`FALSE`）、`PIDL`（`pidl`／`null`）。
  - `cancelled`：是否取消；`error`：非取消的失败原因（没有则为 null）。
  - `paths`：选中的全部路径（多选全列）；`files`：逐项的 `path`、`name`（最终文件名）、`extension`（不含点）、`exists`、`is_dir`。
  - `filter_index`：返回时选中的过滤器（从 1 起；没设过滤器或取消为 null）。
  - `extras`：各方式特有的返回数据。`gofn`：`buffer_layout`（`single-path` 或多选的 `dir+names`）、`buffer_segments`（缓冲区拆出的各段）、偏移量处的文本 `text_at_file_offset`／`text_at_extension_offset`（偏移数值本身随路径长度变化，不记）、`flags_out`；`sbf`：`display_name`、`flags`。
- `meta`：进程号、对话框耗时。每次都不同，比对时忽略。

## 日志

`logs/yyyy-MM-dd HH-mm-ss.log`（与调试观测框架同一约定，目录已被 `.gitignore` 忽略）。**一次启动一个文件**：`run` 的一次进程一个文件；`gui` 窗口里点的所有按钮共用一个文件；`scripts/dialog-baseline.ps1` 把整次批量运行（几十个宿主进程）通过 `--log-file` 汇到同一个文件，并追加驱动动作（输入了什么、点了什么、是否出现覆盖确认）。行格式 `HH:mm:ss.fff +启动后毫秒 内容`，内容含 `request`、`result`（单行 JSON）。目录可用 `--log-dir` 或环境变量 `SFM_LOG_DIR` 改，默认当前目录下的 `logs`。日志里会有夹具的绝对路径（含用户名），不要入库。

## 夹具

`dialog-host fixture` 每次**删除并重建** `%TEMP%\sfm-dialog-host\...\fixture`，不读写用户真实目录。宿主只允许在“临时目录之下且路径含 `sfm-dialog-host` 一级”的位置重建，否则拒绝（防止误删）。内容：

```text
docs\ alpha.txt beta.txt gamma.txt 报告.txt existing.txt notes.md image.png photo.jpg noext
docs\sub\inner.txt
empty\
```

宿主本身从不创建或修改被选中的文件，保存场景也只返回路径。

## 基准与比对

脚本：`powershell -File scripts/dialog-baseline.ps1 -Mode <模式>`（Windows PowerShell 5.1；驱动代码 `scripts/dialog-driver.cs`，由脚本编译）。

| 模式 | 作用 | 退出码 |
| --- | --- | --- |
| `list` | 列出全部 45 个用例（方式×场景×驱动动作），不弹框 | 0 |
| `record` | 构建后跑完全部用例，写入 `tests/dialog-baseline/<系统>.json`（`windows10`／`windows11`） | 0 成功；1 有用例执行失败（不写基准） |
| `compare` | 跑用例，与当前系统的基准逐字段比对；`-Out 文件` 保存本次结果 | 0 一致；1 有差异；2 无法判断（该系统没有基准） |
| `diff` | 不弹框：比较 `-Actual 结果文件`（与 `record` 同一格式，如接管后产生的）与基准 | 同上 |
| `selftest` | 不弹框：在基准副本上人为制造 11 种字段差异，证明比对入口能逐字段检出 | 0 全部检出 |

常用参数：`-Bits x64,x86`（默认两种都跑）、`-Filter <用例 id 正则>`、`-SkipBuild`、`-Baseline 文件`、`-Perturb`（让驱动在 `ifd-open-single` 里故意选另一个文件，演示真实运行里的差异检出）。

差异输出逐字段列出，不是只报“通过／失败”：

```text
ifd-open-single [x64]
  result.paths[0]
      基准: "{FIXTURE}\docs\alpha.txt"
      实际: "{FIXTURE}\docs\beta.txt"
```

字段路径形如 `result.files[0].extension`、`observed.overwrite_confirm_shown`；数组长度不同表现为某个下标“<缺失>”；整个用例缺失标为 `(整个用例)`。

### 用例

每个用例按 x64 与 x86 各跑一次（共 90 次）。“驱动动作”指自动驱动对对话框做了什么。

| 方式 | 用例（`ifd-`／`gofn-` 前缀同名，除非另注） | 驱动动作 |
| --- | --- | --- |
| `ifd` `gofn` | `open-single` 打开单个 | 在文件名框输入 `alpha.txt`，确定 |
| | `open-initdir-sub` 初始目录是子目录 | 输入 `inner.txt`，确定 |
| | `open-single-owner` 带所有者窗口 | 同 `open-single` |
| | `open-multi` 多选两个 | 输入 `"alpha.txt" "beta.txt"`，确定 |
| | `open-multi-one` 多选模式只选一个 | 输入 `"gamma.txt"`，确定 |
| | `open-multi-cjk` 多选含中文文件名 | 输入 `"报告.txt" "alpha.txt"`，确定 |
| | `open-filter-initial` 初始选第 2 个过滤器 | 输入 `image.png`，确定 |
| | `open-filter-switch` 切换过滤器 | 切到第 3 个过滤器，输入 `notes.md`，确定 |
| | `open-cancel` | 点取消 |
| | `save-default-name` 默认文件名 `draft.txt` | 直接确定 |
| | `save-default-ext` 默认扩展名 txt | 输入 `report`，确定 |
| | `save-typed-ext` 默认扩展名 txt、输入带扩展名 | 输入 `report.md`，确定 |
| | `save-no-default-ext` 不设默认扩展名 | 输入 `data`，确定 |
| | `save-filter-initial` 初始选第 2 个过滤器 | 输入 `data`，确定 |
| | `save-overwrite-yes` 已存在文件、覆盖确认 | 默认名 `existing.txt`，确定，覆盖确认点“是” |
| | `save-overwrite-no` | 同上点“否”，再取消 |
| | `save-overwrite-off` 关闭覆盖确认 | 确定；记录是否仍出现确认框 |
| | `save-cancel` | 点取消 |
| `ifd` | `folder`、`folder-initdir`、`folder-owner`、`folder-cancel` | 输入 `sub` 确定／初始目录 `sub` 直接确定／同 `folder`／取消 |
| `sbf` | `sbf-folder-new`、`sbf-folder-new-initdir`、`sbf-folder-legacy-initdir`、`sbf-folder-owner`、`sbf-folder-cancel` | 编辑框输入完整路径确定／回调预选后直接确定（新样式、旧样式）／同 `folder-new`／取消 |

`observed.overwrite_confirm_shown`／`overwrite_confirm_answer` 是驱动观察到的行为（点确定后是否弹出确认框、怎么答的），与 `request`、`result` 一起参与比对；耗时、窗口标题等驱动细节只写日志，不参与比对。

### 基准文件

`tests/dialog-baseline/windows10.json`（入库）：`os`（系统名、build、版本）、`cases.<用例>.<x64|x86>.{description,request,observed,result}`。夹具绝对路径统一归一化为 `{FIXTURE}`，文件里不含用户名或本机路径。

覆盖状态：

| 系统 | 状态 |
| --- | --- |
| Windows 10 22H2（build 19045）x64 | **已覆盖**：x64 与 x86 宿主各 45 个用例 |
| Windows 11 | **未覆盖**：没有 Win11 环境，没有 `windows11.json`。在 Win11 上 `compare` 会报“未覆盖”并以退出码 2 结束，不会拿 Win10 的基准代替。Win11 统一由 Issue #2 跟踪；有环境后在该机器上 `-Mode record` 即可生成。 |

### 基准里已经看到的系统行为（Windows 10 22H2）

这些是“替代实现应当给出相同返回”的依据，也提示了容易写错的地方：

- 取消：`ifd` 返回 `0x800704C7`；`gofn` 返回 `FALSE` 且 `CommDlgExtendedError` 为 0；`sbf` 返回空 PIDL。
- 保存且设置默认扩展名时，没有扩展名的输入会补上（`report` → `report.txt`）；输入带扩展名则保留（`report.md`）；不设默认扩展名时，即使选了“图片”过滤器，`data` 也不会被补扩展名（`ifd`、`gofn` 一致）。
- 多选：`ifd` 总是返回项目列表；`gofn` 多选模式下选两个文件时缓冲区是“目录＋各文件名”的 `dir+names` 布局（`nFileOffset` 指向第一个文件名），**只选一个时却是完整路径**（`single-path`，与不开多选一致）。
- 覆盖确认：打开保存且文件已存在时出现确认框（`ifd` 与 `gofn` 弹出的是同一种“确认另存为”框，按钮没有标准 ID）；选“否”回到对话框，随后取消得到“取消”结果；`overwrite-prompt=off` 时不出现。
- 过滤器：`filter_index` 从 1 起，`ifd` 与 `gofn` 一致。
- `ifd` 选文件夹、`sbf` 选文件夹在直接确定时都返回当前预选的目录。

## 自动驱动怎么做、边界

- 脚本按**进程号**枚举本测试宿主弹出的可见顶层窗口（类 `#32770`），只在这些窗口的后代控件里按控件 ID 找：文件名／路径框（1148、1152、另存为的 1001、SHBrowseForFolder 的 0x3744）、确定 1、取消 2、过滤器下拉。向文件名框发 `WM_SETTEXT`（“路径＋回车”的等价做法），用 `PostMessage(BM_CLICK)` 点按钮（确定可能弹模态确认框，同步发消息会被卡住）。过滤器切换用给下拉框投递方向键。
- 不枚举、不操作其他进程的窗口，不截图桌面，不用全局键盘输入。
- 脚本运行时对话框会短暂出现在桌面上（全部 90 次约 5 分钟）；需要已解锁的交互桌面。
- 不依赖系统语言：按控件 ID 和出现顺序找控件，不按按钮文字。覆盖确认框的“是／否”按钮没有标准 ID，按出现顺序取第 1、第 2 个按钮，实际点到的按钮文字写进日志。

## 能证明什么、不能证明什么

能证明：

- 在**受控的夹具和规整的调用方式**下，三类调用方式、两种位数的系统对话框的返回值、路径、过滤器序号、文件名／扩展名、取消、覆盖确认等是什么；基准里记录了这些，替代实现回传后能逐字段比较。
- 替代实现对**这里覆盖的调用参数组合**回传的字段与系统一致（或差在哪个字段）。
- x86 与 x64 宿主的区别（如 `OPENFILENAMEW` 的结构布局）是否被正确处理。

不能证明：

- **真实软件的特殊调用方式**：自带钩子（`lpfnHook`、`OFN_ENABLEHOOK`／`OFN_ENABLETEMPLATE`）、`IFileDialogEvents`／`IFileDialogCustomize` 自定义控件、`SetClientGuid`、`SetNavigationRoot`、保存对话框的 `IFileSaveDialog::SetProperties`、`OFN_ENABLESIZING` 等变体、`SHBrowseForFolder` 的 `pidlRoot`、非文件系统项目（库、此电脑）、Chromium／Electron／Qt／Java／.NET 等框架自己的包装，宿主都没覆盖。这些必须由 #19 的真实软件验收确认。
- 对话框**界面外观与交互**（本测试用 `WM_SETTEXT` 和按钮消息驱动系统对话框，不覆盖键盘、鼠标、拖放与高 DPI 表现）；替代对话框自己的界面要另行验收。
- **接管技术本身**（怎样让这些调用显示本应用的选择器）：本宿主只负责发起调用和记录结果，不涉及注入、挂钩或注册。
- Windows 11 的行为（未覆盖，见上）；系统界面语言、权限不足、网络路径、超长路径（`sbf` 的路径缓冲区是 `MAX_PATH`）、云盘占位文件。
- 保存场景里“切换过滤器”：Vista 风格另存为的文件类型下拉框不响应投递的方向键（`ifd` 保存实测如此，`gofn` 保存是同一种对话框，未单独验证），所以保存只覆盖“初始选中第 N 个过滤器”；打开场景的切换（`ifd`、`gofn`）已覆盖。
- 单次自动驱动的时序：偶发的对话框迟到由超时兜底并报告为用例失败，不会静默通过。

## 验证入口

`powershell -File scripts/verify-dialog-host.ps1 [-Quick] [-SkipBuild]`（不含在 `scripts/verify.ps1` 里）：`cargo fmt --check`、`clippy -D warnings`、x64 与 x86 发布构建、`selftest`、带按钮窗口冒烟（点第一个预设按钮、驱动系统对话框、检查窗口里的结果文本和日志）、与基准比对（`-Quick` 只比对 8 个代表用例）。退出码 0 通过、1 失败、2 该系统没有基准（未覆盖）。

## 带按钮窗口

`dialog-host gui`（或直接双击）：11 个预设按钮，每个对应一种方式和场景（现代／旧式的打开、多选、保存、覆盖确认，以及 `SHBrowseForFolder` 新旧样式），下方文本框显示每次调用的完整 JSON，同时写日志。窗口启动时重建夹具，初始目录都是夹具的 `docs`。供人工在替代前后手工操作对比。窗口不声明 DPI 感知，高 DPI 下由系统缩放，这与很多老软件的情形一致。
