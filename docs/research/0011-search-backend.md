# 0011 搜索后端比较与集成契约

对应 Issue #11（交付目录／全盘搜索及可选增强）的第一个检查点。结论先行：

- **基本搜索（自研遍历）始终可用，是唯一被实测验证的后端。**
- **Everything（窗口消息 IPC）作为可选增强接入，但本机没有安装 Everything，真实 IPC 传输未与真实程序联调，也没有“增强 vs 基本”的真实速度对比数据。** 联调与测速列入人工验收清单。没有证据，不强制采用 Everything，也不声称它更快。
- 其余候选（Windows Search 索引、直接解析 NTFS MFT／USN）不采用，理由见下表。

依据：[ADR 0007](../adr/0007-optional-search-backend.md)（可选依赖、基本搜索始终可用）、[ADR 0004](../adr/0004-visible-thumbnails-and-cloud-downloads.md)（不触发云文件下载）。

## 本机核查（只读，2026-11-08）

| 项目 | 结果 | 方法 |
| --- | --- | --- |
| Everything 是否安装 | 未安装：`Program Files`、`Program Files (x86)`、`%LOCALAPPDATA%\Programs` 下均无 `Everything*` | 目录存在性检查，不安装、不下载 |
| Everything 是否在运行 | 否：没有名为 `Everything*` 的进程 | 进程列表 |
| Windows Search 服务（`WSearch`） | 运行中，启动类型自动 | 服务状态，只读 |

“未安装”与“已安装但没运行”在应用里由 `EverythingIpc::probe()` 区分：找不到 Everything 的隐藏窗口时，再看常见安装目录里有没有 `Everything.exe`。

## 候选后端比较

| 后端 | 是否预装／需要什么 | 覆盖范围 | 实时性 | 云文件风险 | 取消／增量 | 结论 |
| --- | --- | --- | --- | --- | --- | --- |
| **A. 自研遍历（基本搜索）** | 无依赖 | 任意目录、任意盘符，含未建索引的位置 | 查询时现读，永远最新 | 只读目录项自带的元数据，不打开文件；跳过未同步到本机的云文件夹 | 工作线程，随时取消，结果分批出现 | **采用，始终可用** |
| **B. Everything（IPC）** | 需单独安装并运行；本机没有 | NTFS 卷整盘索引（它自己用 MFT／USN；非 NTFS 或未索引位置覆盖不全） | 索引实时更新，但会有陈旧项 | 它只返回名称与路径，不读内容 | 一次返回一批；取消只能放弃等待 | **可选增强**（接口已做，未联调） |
| C. Windows Search 索引（`SystemIndex`，OLE DB／`ISearchQueryHelper`） | Windows 自带，本机服务在运行 | 仅限被索引的位置（默认只有用户目录、开始菜单等），不是整盘；用户可关闭或改范围 | 索引有延迟 | 索引可能含云文件的属性，查询本身不下载 | 结果以行集返回，可取消 | 不采用：范围不可控、无法保证“全盘”，不满足全盘搜索的语义 |
| D. 直接解析 NTFS MFT／USN 日志 | 需要管理员权限打开卷 | NTFS | 实时 | 无 | 可以 | 不采用：要求提权，违背“普通权限日常使用”，且仅限 NTFS |
| E. 调用 `es.exe`（Everything 命令行） | 需要额外分发或下载第三方可执行文件 | 同 B | 同 B | 同 B | 进程级取消 | 不采用：不随附也不下载第三方二进制；与 B 同样依赖运行状态，且多一层进程解析 |

选择 A + B 的理由：A 保证 ADR 0007 的底线（不装任何东西也能搜），B 在用户已经装了 Everything 时提供更快的路径。没有数据支持“B 一定更快”——它在大索引、冷缓存的 A 上大概率更快，但这里没有测量，所以不写数字。

## 集成契约

### 引擎边界（`src/search/engine.rs`）

调用方只面对 `run(request, enhancer, cancel, sink)`：阻塞执行一次搜索，事件通过 `sink` 交出，且**恰好一个 `Done`**。

```
Started { backend, notice }   开始，说明采用的后端和降级提示（回退后会再发一次）
Reset                         丢弃此前收到的全部结果（增强中途失败回退时）
Hits(Vec<Hit>)                一批结果（增量）
Progress { dirs, entries }    基本搜索的扫描进度
Done(Summary)                 结束：命中数、扫描规模、跳过与不可访问的统计、是否截断／取消、用时
```

- `Request { query, scope, pref }`：`scope` 为 `Dir(目录)` 或 `AllDisks(根目录列表)`；`pref` 为 `Auto`（有增强且可用就用）或 `BasicOnly`（强制基本搜索，排错与对比速度用，环境变量 `SFM_SEARCH_BACKEND=basic`）。
- 后端选择：`Auto` 时先 `probe()`。`Ready` 才走增强；其余四种状态（`NotInstalled`／`NotRunning`／`IndexLoading`／`Failed`）一律用基本搜索，并给出**各不相同**的提示：未安装只在“搜索能力”里注明（不弹提示条，免得没装的用户每次都被打扰），其余三种是提示条。
- 增强查询中途失败（超时、回复格式异常、通信错误）：发 `Reset`，用基本搜索重新完成同一个查询，提示条写明“查询失败，已改用基本搜索重新搜索”。**绝不把两套结果混在一起，也不把失败显示成“没有找到匹配项”。**
- 增强返回的结果不被直接信任：交回引擎后按本应用自己的 `Query` 规则**二次过滤**、按范围裁剪、**剔除磁盘上已不存在的陈旧索引项**（计入 `Summary::stale_dropped` 并在状态行说明）、去重。所以两种后端的名称／扩展名／路径语义一致。
- 离线／未就绪的磁盘：范围里不存在的根目录不会被悄悄忽略，而是计入 `roots_unavailable` 并在状态行说明；没有任何可用根时界面显示“搜索范围不可访问”，而不是空结果。

### Everything IPC（`src/search/everything.rs`）

- 不使用 Everything SDK 的 DLL，也不调用 `es.exe`：向 Everything 已创建的隐藏窗口（窗口类 `EVERYTHING_TASKBAR_NOTIFICATION`，1.5 alpha 为 `…_(1.5a)`）发 `SendMessageTimeoutW`。
- 状态判定：找不到窗口 → 看安装目录区分 `NotInstalled` 与 `NotRunning`；窗口存在但 2 秒内不回应 → `Failed`；`IPC_IS_DB_LOADED` 返回 0 → `IndexLoading`；否则 `Ready(版本号)`。
- 查询：`WM_COPYDATA`，`dwData = EVERYTHING_IPC_COPYDATAQUERYW(2)`，载荷是 `reply_hwnd／reply_copydata_message／search_flags／offset／max_results` 五个 32 位整数加 UTF-16 查询串；Everything 把结果以 `WM_COPYDATA` 发回我们创建的隐藏回复窗口（`ChangeWindowMessageFilterEx` 放行）。
- 粗筛查询串是本应用语义的**超集**：名称词与通配词原样交给 Everything，扩展名写成 `ext:a;b`，路径词与目录范围依赖“匹配完整路径”标志；目录范围写成 `"<目录>\"` 前缀词。精确语义由本应用二次过滤保证。
- 编码与解码（`encode_query`、`decode_list`、`build_query`）都有字节级单元测试；模拟后端 `MockEnhancer` 可注入状态、结果与故障。

### 尚未验证、需要真实联调的点（风险）

以下内容依据 Everything SDK 公开说明编写，**没有与真实的 Everything 联调**，标记为“待验证”，已写入人工验收清单：

1. `IPC_IS_DB_LOADED`（401）与版本号查询消息的编号来自公开头文件说明，未在真实窗口上核对。
2. `reply_hwnd` 在 64 位进程里按 32 位整数传递（窗口句柄的有效位数在 Windows 上为 32 位）；真实 Everything 是否接受未确认。
3. 回复窗口在搜索线程里创建并在阻塞的 `SendMessageTimeoutW` 期间接收回复，这一收发方式在真实环境下是否稳定、大结果集（数万条）是否被截断，未确认。
4. Everything 1.5 alpha 与 1.4 的窗口类名、IPC 差异只做了窗口类名的兼容探测，未验证 1.5 的新 IPC。
5. 增强对比基本搜索的真实速度：没有数据。

## 查询语法（两种后端共用）

名称子串（默认）、`.扩展名` 或 `ext:a;b`、`path:片段` 或含 `\` `/` 的词匹配完整路径、`*` `?` 通配、引号内按普通名称、多个条件用空格表示 AND，全部不区分大小写。详见 [docs/search.md](../search.md)。

## 测量记录

基本搜索的真实规模与耗时见 [docs/search.md](../search.md) 的“规模与耗时”一节（只记录统计，不记录私人路径与文件名）。
