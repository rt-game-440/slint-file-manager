# 系统入口：用本应用打开文件夹（Issue #17）

本应用可以接管“双击文件夹、双击驱动器、按 Win+E”这三个入口。默认**不接管**；只有用户在“更多 → 设置…”里打开开关才会写注册表。方案与证据见 [研究 0016](research/0016-system-entry-takeover.md)。

本文也随分发包提供：程序被删除或移走、没法在应用里恢复时，按“手工恢复”一节操作。

## 1. 行为

| 入口 | 启用后 | 说明 |
| --- | --- | --- |
| 双击文件夹（桌面、资源管理器窗口、文件对话框以外的地方） | 本应用打开该文件夹 | 资源管理器窗口里双击文件夹也会改到本应用打开（`Directory` 默认动词的必然结果，已接受） |
| 双击驱动器（“此电脑”里的 `C:` 等） | 本应用打开该驱动器 | `C:\` 的反斜杠由本应用按原始命令行解析，不会丢 |
| Win+E | 激活已有窗口；没有窗口时启动并按会话恢复 | |
| 右键文件夹或驱动器 | 粗体默认项为“用 Slint 文件管理器打开” | 系统的“打开”仍是资源管理器 |
| 此电脑、回收站、控制面板、网络等虚拟位置 | 不变，仍由 Windows 打开 | 不接管 `Folder`，也从不改写系统的 `open` 动词 |
| “在文件夹中显示”（浏览器下载、VS Code 等） | 不变 | 留给 #18 |

收到打开请求时（见 [panes-tabs-session.md](panes-tabs-session.md#外部请求打开目录)）：

- 已有标签打开着同一路径：切到该标签（先左后右）；否则在当前面板新开标签。然后把窗口还原并切到前台。
- 目标是文件：打开它所在的文件夹并选中它。`explorer` 风格的 `/select,<项目>` 同样处理。
- 目标不存在或无权访问：弹出“无法打开文件夹”并说明原因，不新建标签；已经有对话框时改在状态栏提示。
- 打开前关闭搜索、预览与设置面板，让用户看到新标签。

## 2. 设置面板

“更多 → 设置…”打开，Esc 或右上角 × 关闭。“系统入口”一节有开关“用本应用打开文件夹”和状态框：

| 状态 | 含义 | 可用操作 |
| --- | --- | --- |
| 未接管 | 三个入口仍由资源管理器打开 | 打开开关 → 确认对话框（说明影响，并列出将被替换的现有设置）→ 启用 |
| 已接管 | 注册表里的值全部是本应用写入的、且指向当前程序 | 关闭开关 → 直接恢复原设置 |
| 冲突 | 部分值被别的程序改动了 | “重新接管”或“恢复原设置”（恢复时不覆盖别的程序的改动，并如实报告） |
| 关联失效 | 登记的程序路径不是当前程序（程序被移动、解压到别处，或旧位置已删除） | “修复”（改为当前程序）或“恢复原设置” |
| 不可用 | 没有数据目录（`--no-session` 等）无法保存备份，或注册表根被测试变量拒绝 | 无 |

**启动自检**：每次启动检查一次。关联失效时弹出“文件夹打开方式指向了旧位置”，可选“修复／恢复原设置／以后再说”；冲突时在状态栏常驻一条说明。

## 3. 写入的注册表项

只写当前用户的 `HKEY_CURRENT_USER\Software\Classes`（下表省略这个前缀），不写 HKLM，不需要管理员权限。`<exe>` 为当前程序的完整路径。

| 键 | 值 | 写入内容 |
| --- | --- | --- |
| `Directory\shell\slintfm.open` | （默认） | `用 Slint 文件管理器打开` |
| `Directory\shell\slintfm.open\command` | （默认） | `"<exe>" --entry "%1"` |
| `Directory\shell` | （默认） | `slintfm.open`（设为默认动词） |
| `Drive\shell\slintfm.open` | （默认） | `用 Slint 文件管理器打开` |
| `Drive\shell\slintfm.open\command` | （默认） | `"<exe>" --entry "%1"` |
| `Drive\shell` | （默认） | `slintfm.open` |
| `CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command` | `DelegateExecute` | 空字符串 |
| 同上 | （默认） | `"<exe>" --entry` |

写入顺序是先建动词、最后切换默认动词；恢复时倒序。启用后会通知外壳刷新关联（`SHChangeNotify(SHCNE_ASSOCCHANGED)`）。

### 备份

- 写入前把上表每个值的原值（含类型和原始字节；原来没有就记为“不存在”）和原来不存在的键记到数据目录的 `entry-takeover-backup.json`（默认 `%LOCALAPPDATA%\slint-file-manager\`）。**备份写不进去就不写注册表**；写注册表中途失败会立即回滚。
- 恢复时只还原“仍是本应用写入的值”；被别的程序改过的值保留并报告为冲突；只删除本应用建的空键。恢复干净后删除备份文件。
- “重新接管”“修复”沿用旧备份里的原值，不会把本应用自己的旧值当成原值。
- 备份文件损坏时移到 `entry-takeover-backup.corrupt-<时间>.json`，不猜测原值；这时恢复只删除本应用写入的值（原来的默认动词无从得知，回到系统默认），并在界面上说明。

## 4. 手工恢复（不依赖本程序）

适用于程序已删除或移走、双击文件夹报“找不到应用程序”等情况。在“命令提示符”（不需要管理员）里执行：

```bat
reg delete "HKCU\Software\Classes\Directory\shell\slintfm.open" /f
reg delete "HKCU\Software\Classes\Drive\shell\slintfm.open" /f
reg delete "HKCU\Software\Classes\Directory\shell" /ve /f
reg delete "HKCU\Software\Classes\Drive\shell" /ve /f
reg delete "HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}" /f
```

注意：

- 后两行删除的是 `Directory\shell`、`Drive\shell` 的**默认值**（`/ve`），不是整个键；这两个键里常有别的程序的菜单项，不要整键删除。
- 启用前这两个默认值如果原本有内容（例如很多机器上 `Drive\shell` 的默认值是 `none`，或者装过别的文件管理器），可以从备份文件里查到原值再写回，例如：`reg add "HKCU\Software\Classes\Drive\shell" /ve /d none /f`。备份文件里 `"key"` 为 `Drive\shell`、`"name"` 为空串的那一项，`"original"` 记录原值（`null` 表示原来没有）。
- `CLSID\{52205fd8-…}` 这一行只在它是本应用建的时候删：备份文件的 `"missing_keys"` 里有这个键（即启用前 `HKCU\Software\Classes\CLSID` 下没有它，绝大多数机器如此）。如果原来就有，只删除其中的 `shell\opennewwindow\command` 的默认值和 `DelegateExecute`，或按备份写回原值。
- 执行后注销重新登录（或重启资源管理器）即可生效。

检查是否已恢复：`reg query "HKCU\Software\Classes\Directory\shell" /ve`、`reg query "HKCU\Software\Classes\Drive\shell" /ve` 不再显示 `slintfm.open`；`reg query "HKCU\Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}"` 报找不到。

## 5. 启动参数与单实例

- `--entry [目标]`：接管命令使用的参数，必须是最后一个参数；目标按**原始命令行**解析（`sysentry::args`），所以 `"C:\"`、带空格和中文的路径都能正确处理。没有目标（Win+E）表示激活窗口。
- 没有任何 `--` 开关时，参数按 explorer 风格解析：裸路径、`/select,<项目>`、`/e,`、`/root,` 等。
- 此电脑、控制面板等虚拟位置（`::{CLSID}`、`shell:` 名称）用显式的 `open` 动词交回资源管理器，不会回到本应用而循环。
- **单实例**：有数据目录时每个数据目录一个实例。主实例占用命名管道 `\\.\pipe\slint-file-manager-<会话号>-<数据目录哈希>`（只接受本机连接）；第二个进程把请求（打开位置或激活窗口）用一行 JSON 发过去，收到答复后退出。请求只有“激活”和“打开文件夹（可选选中项）”两种，不能执行命令。
- 主实例无响应：转发超时（默认 5 秒）后第二个进程独立启动，状态栏说明原因，**不保存会话**（避免覆盖主实例的会话）。
- 数据目录不同（`--data-dir=`、`SFM_DATA_DIR`）就是不同实例，互不转发。

## 6. 测试专用的环境变量

| 变量 | 作用 |
| --- | --- |
| `SFM_TEST_REG_ROOT` | 把注册表根从 `HKCU\Software\Classes` 换成测试键。**只接受** `HKCU\Software\slint-file-manager-test\<名称>` 之下的路径，其他值一律拒绝，此时接管功能显示“不可用”，绝不回退到真实根。测试根下不通知外壳刷新。 |
| `SFM_INSTANCE_TIMEOUT_MS` | 单实例转发的超时（毫秒），默认 5000。 |

- `--print-instance-name`：输出该数据目录对应的管道名后退出。
- 单元测试（`cargo test --lib sysentry`）只用内存实现和 `HKCU\Software\slint-file-manager-test\<pid>-<纳秒>`，结束时整棵删除。
- harness 交互脚本（`scripts/harness.ps1 run` 的“系统入口”一节）用测试根，截图场景 `settings-*` 用内存实现。
- 真实程序验证 `scripts/verify-instance.ps1`（已含在 `scripts/verify.ps1`）：临时数据目录、`SFM_TEST_REG_ROOT` 指向不存在的测试键（只读不建）。

## 7. 限制与未验证

- 开发机上**没有做过任何真实接管**；真实入口（双击、右键、Win+E、移动程序后的自检）要在用户新建的 Windows 测试账户里按 [验收清单](checklist/checklist.md) 的 #17 一节验证。
- Windows 11 没有环境，全部未实测（研究 0016 第 7 节）。
- 程序被移走或删除后、再次启动本应用之前，双击文件夹会报错；这是 zip 分发、没有安装程序的已知风险，靠启动自检和本文的手工恢复处理。
- 主实例无响应时的独立窗口不保存会话；主实例恢复响应后不会自动合并。
