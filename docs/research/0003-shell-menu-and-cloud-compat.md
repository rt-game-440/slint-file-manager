# 系统右键菜单、NanaZip 与云占位文件兼容边界（Issue #3 原型结论）

日期：2026-11-07。原型代码：[`prototypes/shell-cloud-probe`](../../prototypes/shell-cloud-probe)（独立 crate，不属于主程序，`windows` 0.62.2 与主工程锁定版本一致）。

本文只记录在本机实测过的结论。没有实测的环境一律写作“未覆盖”，不能据此推断兼容。云盘部分只记录匿名汇总，不记录用户文件名和私人路径。

## 1. 测试环境

| 项目 | 版本 | 核查方式（只读） |
| --- | --- | --- |
| Windows | Windows 10 专业工作站版 22H2，10.0.19045.6456，x64 | `RtlGetVersion`、注册表 `DisplayVersion`/`UBR` |
| NanaZip | 7.0.1882.0（MSIX 包 `40174MouriNaruto.NanaZip`） | `Get-AppxPackage`（Windows PowerShell 5） |
| OneDrive（个人版） | 26.198.1011.0006（同步根提供方信息与安装版本一致） | 卸载注册表、`CfGetSyncRootInfoByPath` |
| iCloud | 包 `AppleInc.iCloud` 15.10.39.0；同步根提供方版本 15.10.0（iCloud 云盘、iCloud 照片两个同步根） | `Get-AppxPackage`、`CfGetSyncRootInfoByPath` |
| 本机其他右键扩展 | 火绒、QQ、Tailscale、Zed、LocalSend 等 | 菜单枚举结果 |

本机同时装有 OneDrive 和 iCloud，两者的客户端都在运行并已登录。核查过程只读：没有安装、登录、修改系统设置或注册表。

两家同步根的提供方策略（`CF_SYNC_ROOT_STANDARD_INFO`）：

| 同步根 | 水合策略 | 填充策略 | 文件数／仅在线 |
| --- | --- | --- | --- |
| OneDrive | FULL，修饰 0x9（VALIDATION_REQUIRED + ALLOW_FULL_RESTART） | ALWAYS_FULL | 4552 / 4547 |
| iCloud 云盘 | FULL，修饰 0x4（AUTO_DEHYDRATION_ALLOWED） | ALWAYS_FULL | 41276 / 41266 |
| iCloud 照片 | FULL，修饰 0xC | ALWAYS_FULL | 1687 / 1686 |

填充策略都是 ALWAYS_FULL，也就是占位条目已经预先写在本地，枚举目录不会向提供方拉取列表。三个同步根中几乎所有文件都处于“仅在线”状态。

## 2. 原生右键菜单与 NanaZip

### 2.1 接入方式（已验证可用）

1. 路径 → `SHParseDisplayName` 得到绝对 PIDL；多选时用 `SHCreateShellItemArrayFromIDLists` 组成 `IShellItemArray`。
2. `IShellItemArray::BindToHandler(BHID_SFUIObject)` → `IContextMenu`，再 QI 出 `IContextMenu2`/`IContextMenu3`（本机两者都可用）。
3. `CreatePopupMenu` + `QueryContextMenu(CMF_NORMAL | CMF_CANRENAME)`。
4. 由一个**自有的隐藏宿主窗口**（与 UI 同一 STA 线程）当菜单 owner。宿主窗口把 `WM_INITMENUPOPUP`、`WM_DRAWITEM`、`WM_MEASUREITEM`、`WM_MENUCHAR`、`WM_MENUSELECT` 转给 `IContextMenu3::HandleMenuMsg2`，没有 IContextMenu3 时退回 `IContextMenu2::HandleMenuMsg`。不转发的话，NanaZip、“发送到”这类延迟填充的子菜单会是空的。
5. `TrackPopupMenuEx(TPM_RETURNCMD | TPM_RIGHTBUTTON)`。返回 0 表示取消，只销毁菜单、释放接口；返回非 0 时用 `CMINVOKECOMMANDINFOEX`（`CMIC_MASK_UNICODE | CMIC_MASK_NOASYNC`，`lpVerb` 传命令偏移）调用 `InvokeCommand`。
6. 用 `DestroyMenu` 销毁菜单，然后释放 IContextMenu 系列接口。

### 2.2 实测结果（夹具目录 `%TEMP%\slint-fm-probe-fixture`，未触碰用户文件）

| 场景 | 结果 |
| --- | --- |
| 单选文本文件枚举 | 51 项（含子菜单项），其中“NanaZip”子菜单 11 个命令，“打开方式”“发送到”“授予访问权限”子菜单也都填充完整 |
| 单选执行 NanaZip“添加到 "a.7z"” | `InvokeCommand` 成功，约 1.7 s 后生成 132 字节文件；文件头是 7z 魔数 `37 7A BC AF 27 1C`，`tar -tvf` 列出 a.txt |
| 多选 3 个文件，执行“添加到 "<目录名>.zip"” | 生成 415 字节 zip，内含 3 个文件 |
| 压缩包文件菜单 | NanaZip 子菜单变为打开／测试／提取等 12 项，“打开方式”中也有 NanaZip |
| 真实弹出菜单 + 键盘导航进入 NanaZip 子菜单并回车选择“提取到 "<名>\"” | 宿主收到 INITMENUPOPUP 2 次、MENUSELECT 9 次，转发成功 14 次；`TrackPopupMenuEx` 返回该命令，执行后解压出 3 个文件 |
| 执行“打开压缩包” | 启动 `NanaZip.Modern.FileManager`，窗口标题是压缩包路径（压缩包内部浏览交给 NanaZip）；随后正常关闭 |
| 执行 CRC-32 | 启动 NanaZip 校验结果窗口（不会自动关闭，属于预期界面） |
| 顶层菜单用 Esc 取消 ×3 | 每次返回 0，没有调用任何命令，进程正常 |
| 进入 NanaZip 子菜单后逐层 Esc 取消 ×5，然后在同一进程执行“添加到 "b.7z"” | 5 次都返回 0；随后的命令照常生成 b.7z。取消不会留下无效状态 |
| 进入“发送到”子菜单后取消 ×2 | 返回 0，进程正常 |
| 云占位文件上的菜单 | OneDrive 显示“始终保留在此设备上／释放空间／在线查看／版本历史记录／共享”，iCloud 显示“Always keep on this device／Free up space／使用 iCloud 云盘共享”，NanaZip 菜单也在。**只枚举菜单不会触发下载**（前后 on_disk=0） |

说明：键盘操作的做法是向本线程的菜单窗口（类名 `#32768`）投递 `WM_KEYDOWN`，并根据 `WM_MENUSELECT` 回报的高亮项决定下一键。这是程序模拟的按键，不等同于真人键盘和鼠标操作；真人操作列入 4 级验收清单。

### 2.3 进程与资源观测

- **NanaZip 扩展不在本进程内加载**：进程模块里没有 NanaZip DLL，说明它的打包 COM 扩展在进程外执行。**在本进程内加载**的有 QQ（`qq_shell_extension_64.dll`）、火绒（`HRShell.dll`、`HrShredShell.dll`）、OneDrive（`FileSyncShell64.dll`）、`ntshrui.dll`。
- **GDI 对象增长**：第一次构建菜单后增加约 57 个 GDI 对象（外壳初始化），之后每构建一次菜单稳定增加 3～4 个（30 轮后共 +173）。只调用 `QueryContextMenu`、不遍历子菜单时，每轮仍增加约 3 个。本原型自己不创建 GDI 对象，所以增长来自第三方处理程序为菜单项设置的位图（每次菜单约 9～12 个带位图的项）。具体是哪个扩展没有定位，因为禁用扩展需要改注册表，本次不做。USER 对象没有增长。
- **verb 字符串不可靠**：OneDrive 处理程序对 `GCS_VERBW` 返回的是乱码（把 ANSI 写进了宽字符缓冲区），NanaZip 的项没有 verb。所以只能对系统规范 verb（open、copy、delete、properties……）按 verb 匹配，第三方命令应按“菜单路径 + 文本”识别，或者直接交给用户选择。

### 2.4 对 #9 的实施建议

1. 照搬 2.1 的流程。宿主用**独立的隐藏 owner 窗口**承接菜单消息，不必子类化 winit/Slint 的主窗口。菜单显示位置用屏幕坐标计算。
2. 以 `TrackPopupMenuEx` 的返回值为准：0 表示取消，非 0 才调用命令。命令使用 `CMIC_MASK_NOASYNC`，调用后继续正常泵消息。NanaZip 的命令会在独立进程里完成，本应用无需等待。
3. **失败回退**：
   - 拿不到 `IContextMenu`（路径失效、权限问题）时，提示错误，不显示空菜单。
   - 菜单里没有“NanaZip”子菜单（未安装或被禁用）时，不伪造压缩命令，提示“未检测到 NanaZip”。压缩包的“打开”交给 `ShellExecuteEx` 默认关联（本机“打开方式”里同时有 NanaZip 和资源管理器）。
   - `InvokeCommand` 返回失败时，如实提示，不当作成功。
4. **风险**：
   - 第三方扩展在进程内运行，崩溃或卡死会拖垮本应用；GDI 每次增长 3～4 个，按 Windows 默认每进程 10000 的上限，约 2500 次菜单后会耗尽。#9 应在开发构建中记录 GDI 计数。如果实际使用中增长不可接受，再考虑把菜单放到辅助进程里承载（这次没有实现，也没有验证）。
   - `TrackPopupMenuEx` 是模态循环，期间 Slint/winit 的事件回调可能被重入或暂停。这一点只能在 #9 的产品窗口里验证，本原型没有覆盖。
   - UI 线程必须是 STA。winit 是否已经用 OLE 初始化主线程，需要在 #9 中确认。
5. 菜单项识别：NanaZip 命令按“NanaZip > 文本”匹配，文本会随所选文件名变化（如“添加到 "a.7z"”）。不能硬编码 ID，ID 每次都会变。

## 3. 云占位文件的无下载浏览边界

### 3.1 状态识别

- 默认情况下，未打包进程的占位符兼容模式是 `PHCM_APPLICATION_DEFAULT`，系统会**伪装**占位文件：枚举结果里没有 `REPARSE`、`OFFLINE` 位，也读不到云重解析标记，只保留 `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`（0x400000）。
  - 凭这一位已经能识别“仅在线”。
  - 但“本地可用”的云文件看起来和**普通文件完全一样**（属性 0x20）。
- 进程调用 `RtlSetProcessPlaceholderCompatibilityMode(PHCM_EXPOSE_PLACEHOLDERS)` 后可以看到真实属性：仅在线文件是 `REPARSE|OFFLINE|RECALL_ON_DATA_ACCESS`（多数还带 SPARSE），重解析标记是 `0x9000401A`（两家都是这个值；OneDrive 少数文件是 `0x9000601A`）。
- 用 `FILE_READ_ATTRIBUTES + FILE_FLAG_OPEN_REPARSE_POINT` 打开句柄，再调用 `CfGetPlaceholderInfo(STANDARD)`，可以读到 `OnDiskDataSize`、`PinState`、`InSyncState`，**不会触发下载**。本次就用这个值和 RECALL 位的变化来判断是否发生了水合。
- 少数 `PINNED|UNPINNED` 同时置位的隐藏系统文件（推测是 desktop.ini）应当作特殊文件，不归入“始终保留”。

### 3.2 取缩略图方式与下载观测

每次调用前后都读取占位信息。调用结束后等待 3～20 s，再检查一次被测文件，并重新枚举整棵目录树做差异比较：

- 被测文件状态变化，归因于本应用；
- 未被测的文件状态变化，归因于同步客户端自己。

| 方式 | OneDrive | iCloud 云盘 | iCloud 照片 | 水合 |
| --- | --- | --- | --- | --- |
| `IShellItemImageFactory::GetImage(SIIGBF_INCACHEONLY｜SIIGBF_THUMBNAILONLY)` 批量（仅在线） | 400 个混合类型命中 5 个；1500 个图片／视频命中 5 个 | 994 个命中 0 个 | 1500 个命中 663 个 | 全部 0 |
| `SIIGBF_ICONONLY`（每家 200 个） | 全部得到图标 | 全部得到图标 | 全部得到图标 | 0 |
| `SHGetFileInfoW(SHGFI_USEFILEATTRIBUTES)`，仅按扩展名（每家 200 个） | 全部得到系统图标索引 | 同左 | 同左 | 0 |
| `SIIGBF_THUMBNAILONLY`（允许提取，每类最多 10 个最小文件，覆盖 jpg/png/webp/pdf/docx/mp4/heic/jpeg/mov） | 全部返回 `WTS_E_EXTRACTIONPENDING`（0x8004B205）；**之后再用 INCACHEONLY 查询，webp/pdf/docx 10/10 命中**，说明 OneDrive 在后台把服务器端缩略图放进了缓存 | jpeg 6/10、pdf 4/10、docx 4/5、mov 1/1 当场得到缩略图（每张约 0.3～1 s，由提供方给出）；webp 0/10，其余返回 `WTS_E_FAILEDEXTRACTION` | 各类型全部当场得到缩略图 | 全部 0 |
| 整树差异（每轮） | — | — | — | 未被测文件的变化 0 个 |

INCACHEONLY 未命中时返回 `STG_E_FILENOTFOUND`（0x80030002），可以据此判定“缓存中没有”，然后回退到图标。

**本地已有内容**：这个状态本机原本没有，于是用“显式读取”人为制造了 3 个样本，测完已经恢复，见 3.3。

- OneDrive 样本：第一次 THUMBNAILONLY 返回 PENDING，随后 INCACHEONLY 命中。
- iCloud 云盘两个样本：THUMBNAILONLY 都返回 `WTS_E_DATAFILEUNAVAILABLE`（0x8004B204），INCACHEONLY 未命中。也就是说，在 iCloud 云盘里，即使内容已经在本地，也没能通过外壳拿到缩略图。原因未知，见“仍未知事项”。

### 3.3 明确打开／下载的路径（允许获取内容）

| 路径 | OneDrive | iCloud 云盘 |
| --- | --- | --- |
| 显式读取全部内容（模拟“打开”） | 读取约 1.0 s；on_disk 0 → 文件大小，RECALL 位清除 | 两个样本都发生同样的水合，各约 0.7～1.0 s |
| 在本应用的宿主中执行提供方菜单“始终保留在此设备上／Always keep on this device” | 2 s 内完成下载，状态变为 PINNED | 同左 |
| 执行提供方菜单“释放空间／Free up space” | 内容释放（on_disk=0），文件仍在，**不会删除云端内容** | 同左 |

测试后用 `attrib +U -P` / `attrib -U` 把所有样本恢复成原来的“仅在线、未固定”状态，并逐个核对了占位信息。

### 3.4 对 #14 的实施建议

1. **浏览和滚动时**只用 `SIIGBF_INCACHEONLY | SIIGBF_THUMBNAILONLY`。返回 `STG_E_FILENOTFOUND` 或其他失败时，用 `SIIGBF_ICONONLY` 或 `SHGetFileInfoW(SHGFI_USEFILEATTRIBUTES)` 显示类型图标。两家实测都不会触发水合。
2. **请求提供方缩略图**（可选，ADR 0004 允许“云盘已提供的缩略图”）：对首屏中缓存未命中的仅在线文件调用一次 `SIIGBF_THUMBNAILONLY`。
   - OneDrive 会返回 PENDING，并在后台把缩略图写入缓存，之后用 INCACHEONLY 轮询或在重绘时再取即可。
   - iCloud 会当场返回提供方缩略图，但每张约 0.3～1 s，必须放在后台线程，并限制并发和数量。
   - 本次样本中这一方式从未触发水合。但它仍然会调用外壳和提供方的提取逻辑，样本只覆盖了上面列出的类型，所以 #14 应在开发构建中保留水合观测（3.2 的方法），不要把它当作无条件安全。
3. **状态识别**：用 `RECALL_ON_DATA_ACCESS` 判断“仅在线”。要区分“本地可用的云文件”和“普通文件”，可以用 `CfGetSyncRootInfoByPath` 判断目录是否在同步根内，再对单个文件调用 `CfGetPlaceholderInfo`；也可以让进程改用 `PHCM_EXPOSE_PLACEHOLDERS`。后者会改变本进程所有文件 API 看到的属性，影响 std::fs 的行为，需要在 #14 中评估，本次不推荐默认开启。
4. **iCloud 云盘中已在本地的文件**：外壳给不出缩略图时，可以确认 `OnDiskDataSize == 文件大小` 且没有 RECALL 位，再由本应用自己解码本地内容生成缩略图。这时读取的是本地已有数据，不会下载。这个思路还没有实现、没有验证。
5. **明确打开**：用 `ShellExecuteEx` 打开，或者执行提供方菜单。显示“需要下载”的状态，并接受同步客户端的下载延迟。“释放空间”不是删除，界面文案要与“删除”严格区分。
6. 菜单统一走第 2 节的原生菜单，提供方命令（保留、释放、共享、版本历史）可以直接复用。

## 4. 未覆盖与仍未知事项

- **Windows 11**：没有环境，与 #2/ADR 0008 相同。Win11 的现代菜单（IExplorerCommand）与经典 `IContextMenu` 路径的关系，以及 NanaZip 在 Win11 上的承载情况，全部未验证。
- **真人键盘和鼠标操作**：菜单的导航和取消用的是程序投递的按键消息，真人操作的观感和焦点行为列入 4 级验收清单。
- **NanaZip 不可用时的回退**：未安装或禁用 NanaZip 需要卸载或修改设置，本次没有复现，只给出了回退设计。
- **NanaZip 其他版本、OneDrive 商业版、其他 iCloud 版本**：未覆盖。
- **离线、同步客户端退出、占位状态在浏览中变化**：需要断网或退出用户的同步客户端，本次没有做，留给 #14。
- **iCloud 云盘中已在本地的文件拿不到外壳缩略图**（`WTS_E_DATAFILEUNAVAILABLE`）的原因：只测了 2 个样本，原因未知。
- **THUMBNAILONLY 请求的网络开销**，以及在大目录中批量请求时提供方是否限流：未测量。
- **右键菜单 GDI 增长的具体来源扩展**：未定位。
- **水合的发起者**：Cloud Filter API 不会把发起进程告诉第三方观察者，本次只能用“被测集合与未触碰集合”的差异来归因。

## 5. 与现有 ADR 的关系

- ADR 0004（只用已有缩略图、浏览不下载）：实测结论**一致**。补充两点：OneDrive 和 iCloud 都存在“提供方给出、不需要原文件”的缩略图来源；iCloud 云盘中已在本地的文件需要本应用自己生成缩略图。
- ADR 0005（压缩包交给 NanaZip）：在 Windows 10 + NanaZip 7.0.1882.0 上**可以承载**，单选、多选、子菜单、执行和取消都通过了实测。
- ADR 0006（文件操作可以使用系统界面）：原生菜单里的复制、删除、属性等命令本来就会弹出系统界面，**不冲突**。
- 没有发现与现有 ADR 冲突的结论。

## 6. 复现命令

在 `prototypes/shell-cloud-probe` 目录下执行 `cargo build --jobs 6`，可执行文件在 `target\debug\shell-cloud-probe.exe`：

```powershell
$exe = 'prototypes\shell-cloud-probe\target\debug\shell-cloud-probe.exe'
$fx  = "$env:TEMP\slint-fm-probe-fixture"   # 自行创建 a.txt / b.txt / c.txt
& $exe env
& $exe menu --items "$fx\a.txt" --mode list
& $exe menu --items "$fx\a.txt" --mode invoke --match 'NanaZip>添加到 "a.7z"' --wait-file "$fx\a.7z"
& $exe menu --items "$fx\a.txt|$fx\b.txt|$fx\c.txt" --mode invoke --match 'NanaZip>.zip"' --wait-file "$fx\slint-fm-probe-fixture.zip"
& $exe menu --items "$fx\slint-fm-probe-fixture.zip" --mode track-select --match 'NanaZip>提取到 "' --wait-file "$fx\slint-fm-probe-fixture\b.txt"
& $exe menu --items "$fx\b.txt" --mode track-cancel --match 'NanaZip' --repeat 5 --after-invoke 'NanaZip>添加到 "b.7z"' --wait-file "$fx\b.7z"
& $exe cloud-info <同步根>
& $exe cloud-scan <同步根> [--expose]
& $exe thumb-scan <同步根> --mode incache|thumbonly|icononly|exticon --class online --limit 200 [--smallest] [--settle 秒]
& $exe placeholder <文件>
& $exe read-file <文件>        # 会下载该文件，仅用于明确打开路径的验证
```

`thumb-scan` 会把被测文件的相对路径打印到标准输出，仅用于本地日志（`logs/` 已被 git 忽略），不要提交。
