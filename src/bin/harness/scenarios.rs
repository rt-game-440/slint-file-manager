//! 命名场景：截图与动画采集的统一入口，全部基于真实浏览界面与样例目录（见 fixture.rs）。
//! 新增界面状态时在这里追加场景，不要为场景另建抽象层。

use crate::ctx::{Ctx, Opts, Res};
use crate::fixture;
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementRoot};
use serde_json::{Value, json};
use slint::platform::Key;

pub struct Scenario {
    pub name: &'static str,
    /// 场景内容（给 3 级审阅材料用）
    pub content: &'static str,
    /// 预期效果（给 3 级审阅材料用）
    pub expected: &'static str,
    /// 文字缩放（系统“文字大小”）；与截图的设备缩放相互独立
    pub text_scale: f32,
    /// 是否完成首次目录读取（false 时停在“加载中”）
    pub load: bool,
    /// 是否双栏（旧场景保持单栏，构图与基线可比）
    pub split: bool,
    pub setup: fn(&Ctx) -> Res<()>,
}

/// 逐帧采集时被跟踪的元素。
pub const TRACKED: &[(&str, &str)] = &[("loading", "BrowserPane::loading-indicator")];

impl Ctx {
    /// 按无障碍角色与标签查找唯一元素（Text 会以自身文字作为标签，需按角色区分）。
    /// 双栏时两个面板里会有同名元素（后退、地址栏、同名文件……），此时只在活动面板内查找；
    /// 要指定面板用 [`Ctx::find_in`]。
    pub fn find(&self, role: AccessibleRole, label: &str) -> Res<ElementHandle> {
        let mut v = self.find_all(role, label);
        match v.len() {
            1 => Ok(v.remove(0)),
            0 => Err(format!("找不到 {role:?}：{label}").into()),
            _ => {
                let p = self.ctl.ws().active_pane();
                self.find_in(p, role, label)
            }
        }
    }

    /// 全部同角色同标签的可见元素。
    pub fn find_all(&self, role: AccessibleRole, label: &str) -> Vec<ElementHandle> {
        self.win
            .root_element()
            .query_descendants()
            .match_accessible_role(role)
            .find_all()
            .into_iter()
            .filter(|e| e.accessible_label().is_some_and(|l| l == label))
            .collect()
    }

    pub fn button(&self, label: &str) -> Res<ElementHandle> {
        self.find(AccessibleRole::Button, label)
    }

    /// 当前视图（网格或列表）中的文件项。
    pub fn item(&self, name: &str) -> Res<ElementHandle> {
        self.find(AccessibleRole::ListItem, name)
    }

    /// 进入地址编辑（对活动面板的地址栏触发默认操作，窄面板里路径分段占满地址栏时也可靠），输入文字并回车。
    pub fn enter_address(&self, text: &str) -> Res<()> {
        self.button("地址栏")?.invoke_accessible_default_action();
        self.type_text(text);
        self.special(Key::Return, &[]);
        Ok(())
    }

    pub fn open_folder(&self, name: &str) -> Res<()> {
        self.double_click(&self.item(name)?);
        self.pump();
        Ok(())
    }
}

fn grid_selected(c: &Ctx) -> Res<()> {
    c.click(&c.item("图片003.webp")?);
    Ok(())
}

fn grid_multi(c: &Ctx) -> Res<()> {
    c.click(&c.item("图片002.webp")?);
    c.shift_click(&c.item("图片005.webp")?);
    c.ctrl_click(&c.item("说明.md")?);
    c.ctrl_click(&c.item(fixture::LONG_PDF)?);
    Ok(())
}

fn list_sorted(c: &Ctx) -> Res<()> {
    c.click(&c.button("列表视图")?);
    c.click(&c.item("数据表.xlsx")?);
    c.ctrl_click(&c.item("视频片段.mp4")?);
    c.click(&c.button("按大小排序")?);
    c.click(&c.button("按大小排序")?); // 第二次点击切换为递减
    Ok(())
}

fn empty_folder(c: &Ctx) -> Res<()> {
    c.open_folder(fixture::EMPTY)
}

fn not_found(c: &Ctx) -> Res<()> {
    c.enter_address(&fixture::path("不存在的文件夹").to_string_lossy())?;
    c.pump();
    Ok(())
}

fn access_denied(c: &Ctx) -> Res<()> {
    c.open_folder(fixture::DENIED)
}

fn not_a_folder(c: &Ctx) -> Res<()> {
    c.enter_address(&fixture::path(fixture::LONG_PDF).to_string_lossy())?;
    c.pump();
    Ok(())
}

fn loading(c: &Ctx) -> Res<()> {
    c.advance(600); // 越过 150ms 延迟与淡入
    Ok(())
}

fn long_path(c: &Ctx) -> Res<()> {
    c.enter_address(&fixture::deep_path(fixture::DEEP.len()).to_string_lossy())?;
    c.pump();
    c.click(&c.item("结论.txt")?);
    Ok(())
}

fn address_edit(c: &Ctx) -> Res<()> {
    c.click_right_edge(&c.button("地址栏")?, 12.0);
    c.advance(16);
    Ok(())
}

fn sort_menu(c: &Ctx) -> Res<()> {
    c.click(&c.button("排序方式")?);
    c.advance(16);
    Ok(())
}

fn text_scale(c: &Ctx) -> Res<()> {
    c.click(&c.item(fixture::LONG_TXT)?);
    Ok(())
}

fn list_text_scale(c: &Ctx) -> Res<()> {
    c.click(&c.button("列表视图")?);
    c.click(&c.item(fixture::LONG_TXT)?);
    Ok(())
}

// ---------- 双栏与标签（Issue #6） ----------

/// 侧栏：磁盘、收藏（含失效项）、OneDrive；点“照片”收藏进入，星标选中。
fn sidebar_places(c: &Ctx) -> Res<()> {
    c.click(&c.button("照片")?);
    c.pump();
    Ok(())
}

/// 点击失效的收藏：不导航，状态栏说明原因。
fn sidebar_invalid_favorite(c: &Ctx) -> Res<()> {
    c.click(&c.button("旅行素材")?);
    c.pump();
    Ok(())
}

fn dual_default(c: &Ctx) -> Res<()> {
    c.click(&c.item("说明.md")?);
    Ok(())
}

fn abs(p: std::path::PathBuf) -> String {
    p.to_string_lossy().to_string()
}

/// 左面板 3 个标签（当前为“照片”），右面板 2 个标签并活动（当前为样例目录，选中程序.exe）。
fn dual_tabs(c: &Ctx) -> Res<()> {
    c.click(&c.item_in(0, "图片001.webp")?);
    c.ctrl("t");
    c.enter_address(&abs(fixture::path(fixture::PHOTOS)))?;
    c.pump();
    c.ctrl("t");
    c.enter_address(&abs(fixture::path(fixture::EMPTY)))?;
    c.pump();
    c.ctrl("2");
    c.click(&c.item_in(1, "报告2.txt")?);
    c.ctrl("t");
    c.enter_address(&abs(fixture::deep_path(fixture::DEEP.len())))?;
    c.pump();
    c.ctrl("1");
    c.click(&c.item_in(1, "程序.exe")?);
    Ok(())
}

/// 右面板的第二个标签指向不存在的位置：标签保留并带警告图标。
fn dual_unavailable(c: &Ctx) -> Res<()> {
    c.click(&c.item_in(1, "说明.md")?);
    c.ctrl("t");
    c.enter_address(&abs(fixture::path("不存在的文件夹")))?;
    c.pump();
    Ok(())
}

/// 切到单栏后只显示右面板（左面板在内存中保留）。
fn dual_default_right(c: &Ctx) -> Res<()> {
    c.click(&c.item_in(1, "空文件夹")?);
    Ok(())
}

fn single_right(c: &Ctx) -> Res<()> {
    c.click(&c.item_in(1, "说明.md")?);
    c.ctrl("t");
    c.click(&c.button("双栏")?);
    c.pump();
    Ok(())
}

/// 删除与回收站截图用的草稿目录（固定名称、大小、修改时间；位于样例目录之外）。
fn shots_dir() -> Res<std::path::PathBuf> {
    let d = fixture::fresh_dir(&fixture::scratch_dir("shots"))?;
    for (i, (n, size)) in [
        ("季度报告.docx", 184_320u64),
        ("会议纪要.txt", 2_048),
        ("旧照片.webp", 912_384),
        ("备份压缩.zip", 5_242_880),
    ]
    .iter()
    .enumerate()
    {
        fixture::file(&d, n, *size, fixture::time(10 + i as u64))?;
    }
    let sub = d.join("项目资料");
    std::fs::create_dir(&sub)?;
    fixture::file(&sub, "草稿.txt", 100, fixture::time(5))?;
    fixture::dir_time(&sub, fixture::time(6))?;
    Ok(d)
}

fn open_shots(c: &Ctx) -> Res<std::path::PathBuf> {
    let d = shots_dir()?;
    c.enter_address(&d.to_string_lossy())?;
    c.pump();
    Ok(d)
}

fn pick(c: &Ctx, names: &[&str]) -> Res<()> {
    for (i, n) in names.iter().enumerate() {
        let e = c.item(n)?;
        if i == 0 {
            c.click(&e);
        } else {
            c.ctrl_click(&e);
        }
    }
    Ok(())
}

/// 选中后按 Delete 并按 Enter 确认（Manual 加载下操作同步完成）。
fn delete_now(c: &Ctx, names: &[&str]) -> Res<()> {
    pick(c, names)?;
    c.special(Key::Delete, &[]);
    c.special(Key::Return, &[]);
    c.pump();
    Ok(())
}

fn open_bin(c: &Ctx) -> Res<()> {
    c.click(&c.button("回收站")?);
    c.pump();
    Ok(())
}

fn delete_confirm(c: &Ctx) -> Res<()> {
    open_shots(c)?;
    pick(c, &["季度报告.docx", "会议纪要.txt"])?;
    c.special(Key::Delete, &[]);
    c.advance(16);
    Ok(())
}

fn delete_confirm_permanent(c: &Ctx) -> Res<()> {
    open_shots(c)?;
    pick(c, &["备份压缩.zip"])?;
    c.special(Key::Delete, &[Key::Shift]);
    c.advance(16);
    Ok(())
}

/// 三次删除分别记为不同的时间，然后进入回收站并选中一项。
fn recycle_bin_view(c: &Ctx) -> Res<()> {
    open_shots(c)?;
    c.stash.set_clock(fixture::time(100));
    delete_now(c, &["季度报告.docx"])?;
    c.stash.set_clock(fixture::time(130));
    delete_now(c, &["项目资料"])?;
    c.stash.set_clock(fixture::time(160));
    delete_now(c, &["旧照片.webp"])?;
    open_bin(c)?;
    c.click(&c.item("项目资料")?);
    Ok(())
}

fn recycle_bin_empty(c: &Ctx) -> Res<()> {
    open_shots(c)?;
    open_bin(c)
}

fn delete_partial(c: &Ctx) -> Res<()> {
    use slint_file_manager::fileops::{FailKind, Inject};
    let d = open_shots(c)?;
    c.stash.inject(
        d.join("会议纪要.txt"),
        Inject::Delete(FailKind::AccessDenied),
    );
    delete_now(c, &["会议纪要.txt", "旧照片.webp"])
}

fn delete_unsupported(c: &Ctx) -> Res<()> {
    use slint_file_manager::fileops::Inject;
    let d = open_shots(c)?;
    c.stash.inject(d.join("备份压缩.zip"), Inject::Unsupported);
    pick(c, &["备份压缩.zip"])?;
    c.special(Key::Delete, &[]);
    c.advance(16);
    Ok(())
}

fn restore_conflict(c: &Ctx) -> Res<()> {
    let d = open_shots(c)?;
    c.stash.set_clock(fixture::time(100));
    delete_now(c, &["季度报告.docx", "会议纪要.txt"])?;
    // 原位置被同名新文件占用，再还原两项：一项成功、一项冲突
    fixture::file(&d, "会议纪要.txt", 77, fixture::time(200))?;
    open_bin(c)?;
    pick(c, &["季度报告.docx", "会议纪要.txt"])?;
    c.click(&c.button("还原所选项目")?);
    c.pump();
    Ok(())
}

use crate::search::{
    setup_all_disks as search_all_disks, setup_cancelled as search_cancelled,
    setup_denied as search_denied, setup_empty as search_empty, setup_no_disks as search_no_disks,
    setup_results as search_results, setup_scanning as search_scanning,
};

const fn s(
    name: &'static str,
    content: &'static str,
    expected: &'static str,
    setup: fn(&Ctx) -> Res<()>,
) -> Scenario {
    Scenario {
        name,
        content,
        expected,
        text_scale: 1.0,
        load: true,
        split: false,
        setup,
    }
}

pub const STATIC: &[Scenario] = &[
    s(
        "grid-selected",
        "样例目录网格视图，单击选中“图片003.webp”",
        "A2 布局：浅灰侧栏、融合顶栏（排序／视图／双栏／搜索，右上角窗口按钮）、面板顶部标签带（一个标签“样例目录”和“+”，单栏只显示一个面板）、近白内容区；文件夹在前、名称自然排序（报告2 在 报告10 前）；选中项为浅蓝圆角底与蓝色描边；状态栏显示“26 个项目　已选择 1 个项目（…）”",
        grid_selected,
    ),
    s(
        "sidebar-places",
        "展示环境：三个磁盘（系统盘、数据盘、U 盘）、三个收藏（有效／盘符被另一块盘占用／磁盘未连接）、OneDrive；点击“照片”收藏进入",
        "侧栏分组“收藏／此电脑／云盘”清晰；U 盘用 USB 图标；当前位置对应的收藏行为淡蓝高亮；失效的两个收藏为灰字加黄色警示图标；工具栏星标为选中外观；内容区是照片文件夹的 12 张图片",
        sidebar_places,
    ),
    s(
        "sidebar-invalid-favorite",
        "展示环境，点击失效的收藏“旅行素材”（盘符现在是另一块磁盘）",
        "没有导航（仍在样例目录）；状态栏右侧提示“收藏‘旅行素材’已失效（该盘符现在是另一块磁盘），不会改指向其他位置”；失效收藏仍为灰字加警示图标",
        sidebar_invalid_favorite,
    ),
    s(
        "grid-multi",
        "单击图片002、Shift 单击图片005、Ctrl 单击说明.md 与长名 PDF",
        "图片002–005、说明.md、长名 PDF 共 6 项为浅蓝选中；长名 PDF 两行折行后省略；状态栏“已选择 6 个项目”及合计大小",
        grid_multi,
    ),
    s(
        "list-sorted",
        "切到列表视图，选中数据表.xlsx 与视频片段.mp4，点击“大小”表头两次",
        "列表列为名称／修改日期／类型／大小，大小列右对齐、表头“大小”加粗并带向下箭头；文件按大小递减（递减时整体反转，文件夹排到末尾，与资源管理器一致）；两项保持选中，视频片段.mp4 在首行",
        list_sorted,
    ),
    s(
        "empty-folder",
        "双击进入“空文件夹”",
        "内容区居中显示文件夹图标与“此文件夹为空”，不显示重试按钮；后退可用",
        empty_folder,
    ),
    s(
        "not-found",
        "地址栏输入样例目录下不存在的路径",
        "显示“找不到这个位置”、完整路径、说明文字与“重试”按钮，区别于空目录和无权限",
        not_found,
    ),
    s(
        "access-denied",
        "双击进入被拒绝列目录的“无权限”文件夹",
        "显示锁图标、“没有权限访问此文件夹”、路径与说明、“重试”按钮",
        access_denied,
    ),
    s(
        "not-a-folder",
        "地址栏输入长中文名 PDF 的完整路径",
        "显示“这是文件，不是文件夹”、完整路径与“用默认应用打开”按钮；不自动打开文件",
        not_a_folder,
    ),
    Scenario {
        name: "loading",
        content: "启动后目录读取尚未完成（延迟 600ms 观察）",
        expected: "内容区顶部 2px 蓝色进度条与居中“正在加载…”；状态栏“正在读取…”；工具栏与路径栏可见但内容区无文件",
        text_scale: 1.0,
        load: false,
        split: false,
        setup: loading,
    },
    s(
        "long-path",
        "地址栏进入四层长名称子目录并选中结论.txt",
        "路径导航从左侧以“…”收起放不下的层级（整段隐藏，不出现半截文字），最右显示当前层“最终版”且不与按钮重叠；标签标题为“最终版”",
        long_path,
    ),
    s(
        "address-edit",
        "点击路径导航进入地址编辑",
        "路径导航变为可编辑文本框，完整路径被全选（浅蓝选区），文本框有蓝色焦点边框",
        address_edit,
    ),
    s(
        "sort-menu",
        "点击工具栏“排序方式”",
        "排序按钮下方弹出白色圆角菜单：名称（勾选）、修改日期、类型、大小，分隔线后递增（勾选）、递减",
        sort_menu,
    ),
    Scenario {
        name: "text-scale-150",
        content: "系统文字大小 150%（设备缩放仍为截图缩放），网格视图选中长英文名文件",
        expected: "文字、行高、侧栏宽度、工具栏随之放大，没有文字被裁切到不可读；图标最多放大到 1.5 倍；长名折行后省略",
        text_scale: 1.5,
        load: true,
        split: false,
        setup: text_scale,
    },
    Scenario {
        name: "list-text-scale-150",
        content: "系统文字大小 150%，列表视图选中长英文名文件",
        expected: "列表行高与表头随文字放大；名称列省略号截断，日期／类型／大小列完整可读",
        text_scale: 1.5,
        load: true,
        split: false,
        setup: list_text_scale,
    },
    s(
        "search-scanning",
        "在样例目录按 Ctrl+F 搜索“报告”，只送达开始与第一批结果：搜索仍在进行",
        "搜索结果面板覆盖浏览区；顶部显示范围“当前目录及子目录”被选中、范围说明含具体目录；状态行为“正在搜索/扫描… 已找到 N 项”；有“取消搜索”按钮；已出现的结果行有图标、名称与所在目录；工具栏搜索框为蓝色描边并带清除按钮",
        search_scanning,
    ),
    s(
        "search-results",
        "搜索“报告”完成并用向下键选中第一项",
        "状态行“搜索完成：共 3 项（用时… 扫描 N 个文件夹…）”，另有“已跳过 1 个无权限的文件夹”；右上有“打开”“定位所在目录”；第一行为浅蓝选中；能力说明“搜索能力：基本搜索”；底部状态栏显示“搜索结果：3 项”",
        search_results,
    ),
    s(
        "search-empty",
        "搜索一个不存在的名称",
        "结果区居中显示“没有找到匹配项”和修改条件的建议；状态行说明已扫描完毕及用时，不是空白；没有取消按钮",
        search_empty,
    ),
    s(
        "search-denied",
        "搜索只存在于无权限文件夹里的名称“秘密”",
        "没有结果；状态行明确“已跳过 1 个无权限的文件夹”，让用户知道结果不完整的原因，而不是单纯的空白",
        search_denied,
    ),
    s(
        "search-all-disks",
        "切到“全盘”：根目录为样例目录与一个已不存在的磁盘路径",
        "“全盘”按钮为选中态；范围说明列出两个根并声明只按名称搜索、不读内容、不登录云账号；状态行有“1 个位置不可访问（磁盘已移除或未就绪）”；结果仍显示可访问根目录里找到的项",
        search_all_disks,
    ),
    s(
        "search-no-disks",
        "切到“全盘”但没有识别到任何磁盘",
        "结果区显示“搜索范围不可访问”，状态行红字“没有识别到可用的磁盘”；不是空结果",
        search_no_disks,
    ),
    s(
        "search-enhanced",
        "模拟增强后端就绪，返回 3 个有效项和 1 个已被删除的陈旧索引项",
        "能力说明“搜索能力：Everything（增强）”，没有降级提示；3 个结果，状态行说明“已剔除 1 条已不存在的索引项”",
        search_results,
    ),
    s(
        "search-fallback-not-running",
        "模拟增强已安装但未运行",
        "黄色提示条明确“Everything（模拟）已安装但未运行，本次使用基本搜索”；能力说明为基本搜索；结果为基本搜索的完整结果",
        search_results,
    ),
    s(
        "search-fallback-failed",
        "模拟增强查询中途失败（等待回复超时）",
        "黄色提示条说明增强查询失败已回退；能力说明为基本搜索；结果是基本搜索的完整结果，没有混入增强数据，也不是“没有找到匹配项”",
        search_results,
    ),
    s(
        "search-cancelled",
        "扫描中点击“取消搜索”",
        "状态行“已取消：已显示 N 项，未扫描完，结果不完整”；“取消搜索”按钮消失；已出现的结果保留",
        search_cancelled,
    ),
    Scenario {
        name: "search-results-text-scale-150",
        content: "系统文字大小 150% 下的搜索结果",
        expected: "范围按钮、状态行、结果行随文字放大且不被裁切；范围说明与状态行单行省略；按钮不重叠",
        text_scale: 1.5,
        load: true,
        split: false,
        setup: search_results,
    },
    Scenario {
        name: "dual-default",
        content: "首次运行的默认双栏：两个面板都在样例目录，左面板单击选中说明.md",
        expected: "两个面板等宽，中间 1px 分隔线；每个面板顶部各有自己的标签带（一个标签）、路径导航与内容；活动面板（左）顶边有 2px 蓝线、标签为白底，另一面板标签为灰底；右面板无选中项；状态栏只统计活动面板",
        text_scale: 1.0,
        load: true,
        split: true,
        setup: dual_default,
    },
    Scenario {
        name: "dual-tabs",
        content: "左面板 3 个标签（当前“照片”），右面板 2 个标签并为活动面板（当前样例目录，选中程序.exe）",
        expected: "各面板标签带独立：左 3 个、右 2 个，当前标签白底（活动面板）或灰底（非活动面板）；右面板顶边蓝线；左面板内容为照片 12 项，右面板为样例目录；标签带末尾有“+”；标签宽度在窄面板内等分且标题省略",
        text_scale: 1.0,
        load: true,
        split: true,
        setup: dual_tabs,
    },
    Scenario {
        name: "dual-unavailable-tab",
        content: "右面板新建标签并输入不存在的路径",
        expected: "右面板第二个标签保留并显示警告图标；内容区显示“找不到这个位置”与重试按钮；左面板不受影响",
        text_scale: 1.0,
        load: true,
        split: true,
        setup: dual_unavailable,
    },
    Scenario {
        name: "single-right",
        content: "双栏下激活右面板、新建标签后切到单栏",
        expected: "只显示右面板并占满内容区，保留其 2 个标签；“双栏”按钮不再是蓝色选中态；左面板的状态没有丢（由交互脚本断言）",
        text_scale: 1.0,
        load: true,
        split: true,
        setup: single_right,
    },
    Scenario {
        name: "dual-text-scale-150",
        content: "系统文字大小 150% 下的双栏与标签带，右面板活动",
        expected: "标签带、路径导航随文字放大且不被裁切；两个面板仍等宽；窄面板中路径导航以“…”收起；活动面板蓝线可见",
        text_scale: 1.5,
        load: true,
        split: true,
        setup: dual_default_right,
    },
    s(
        "delete-confirm",
        "选中“季度报告.docx”和“会议纪要.txt”后按 Delete，弹出移到回收站的确认",
        "窗口中央偏上出现白色圆角对话框，背后内容区被淡灰遮罩压住；标题“将 2 项移到回收站？”为黑色；正文说明“项目会放进回收站，之后可以在回收站里还原”并列出两个文件名；右下为“取消”（描边）与蓝色主按钮“移到回收站”",
        delete_confirm,
    ),
    s(
        "delete-confirm-permanent",
        "选中“备份压缩.zip”后按 Shift+Delete，弹出永久删除确认",
        "危险样式：标题“永久删除“备份压缩.zip”？”为红色；正文说明直接删除、不进回收站、无法恢复；主按钮是红色“永久删除”，“取消”为描边按钮（与移到回收站的蓝色确认明显不同）",
        delete_confirm_permanent,
    ),
    s(
        "recycle-bin",
        "三个项目在不同时间被删除后，从侧栏进入回收站并选中“项目资料”",
        "侧栏“回收站”为蓝色选中态；内容区顶部有米黄色提示条“回收站：已删除项目的暂存位置，不是普通文件夹…”和“还原所选项目”按钮；列表列为名称／删除日期／原位置／类型／大小，原位置显示草稿目录完整路径；工具栏“删除”“永久删除”为灰色不可用；地址栏显示“回收站”；后退可用、上一级不可用",
        recycle_bin_view,
    ),
    s(
        "recycle-bin-empty",
        "进入没有任何项目的回收站",
        "仍显示米黄色提示条，“还原所选项目”灰色；内容区居中显示“回收站是空的”，不显示重试按钮",
        recycle_bin_empty,
    ),
    s(
        "delete-partial",
        "批量删除“会议纪要.txt”（注入权限不足）和“旧照片.webp”，一项成功一项失败",
        "对话框标题“部分项目没有删除”，正文写明已移到回收站 1 项，未能移到回收站 1 项，点名“会议纪要.txt：没有权限”并说明保持原样；只有蓝色“知道了”；背后列表仍显示“会议纪要.txt”；状态栏为红色“部分完成：1 项已移到回收站，1 项未能移到回收站”",
        delete_partial,
    ),
    s(
        "delete-unsupported",
        "对不支持回收站的位置（注入）按 Delete",
        "只有说明对话框“无法移到回收站”，正文写明没有删除任何项目并指出永久删除（Shift+Delete）无法恢复；只有“知道了”；文件仍在列表中",
        delete_unsupported,
    ),
    s(
        "restore-conflict",
        "回收站里选中两项还原：“季度报告.docx”成功，“会议纪要.txt”因原位置已有同名文件冲突",
        "对话框标题“部分项目没有还原”，写明“会议纪要.txt：原位置已有同名项目，未覆盖”；回收站视图中只剩“会议纪要.txt”；状态栏红色“部分完成：1 项已还原到原位置，1 项未能还原”",
        restore_conflict,
    ),
];

/// 动画场景：在完成首次加载的窗口上，`start` 触发动画。
pub struct AnimScenario {
    pub name: &'static str,
    pub content: &'static str,
    pub expected: &'static str,
    pub start: fn(&Ctx) -> Res<()>,
}

fn start_loading(c: &Ctx) -> Res<()> {
    // 双击进入“照片”但不执行读取：停在加载状态
    c.double_click(&c.item(fixture::PHOTOS)?);
    Ok(())
}

pub const ANIM: &[AnimScenario] = &[AnimScenario {
    name: "loading-appear",
    content: "进入目录后读取一直未完成时，加载提示的出现过程",
    expected: "加载提示不透明度在前 150ms 保持 0（快速目录不闪烁），随后 180ms ease-out 淡入到 1；进度条在顶部循环移动",
    start: start_loading,
}];

pub fn find_static(name: &str) -> Res<&'static Scenario> {
    STATIC.iter().find(|s| s.name == name).ok_or_else(|| {
        format!(
            "未知静态场景 {name}，可选：{}",
            names(STATIC.iter().map(|s| s.name))
        )
        .into()
    })
}

pub fn find_anim(name: &str) -> Res<&'static AnimScenario> {
    ANIM.iter().find(|s| s.name == name).ok_or_else(|| {
        format!(
            "未知动画场景 {name}，可选：{}",
            names(ANIM.iter().map(|s| s.name))
        )
        .into()
    })
}

fn names<'a>(it: impl Iterator<Item = &'a str>) -> String {
    it.collect::<Vec<_>>().join("、")
}

/// 按场景打开窗口并执行 setup。
pub fn open(name: &str, size: (u32, u32), scale: f32) -> Res<Ctx> {
    let sc = find_static(name)?;
    // 名称以 sidebar- 开头的场景使用固定的磁盘／收藏展示环境（见 live::showcase_env）
    let live = if name.starts_with("sidebar-") {
        Some(crate::live::showcase_env(name)?)
    } else {
        None
    };
    let c = Ctx::open(
        size,
        scale,
        Opts {
            text_scale: sc.text_scale,
            load: sc.load,
            split: sc.split,
            live,
            // 名称以 search- 开头的场景使用固定的搜索环境（模拟增强后端、全盘根目录，见 search::showcase_env）
            search: crate::search::showcase_env(name),
            ..Opts::default()
        },
    )?;
    (sc.setup)(&c)?;
    Ok(c)
}

/// 交互脚本：真实浏览流程（导航、地址输入、选择、视图与排序、打开、过期结果），每步记录状态并断言。
pub fn interaction(c: &Ctx) -> Res<(Vec<Value>, Vec<String>)> {
    use crate::dump::extras;
    let mut steps: Vec<Value> = Vec::new();
    let mut fails: Vec<String> = Vec::new();
    let mut step = |c: &Ctx, name: &str, checks: Vec<(bool, &str)>| {
        let mut failed = Vec::new();
        for (ok, msg) in checks {
            if !ok {
                failed.push(msg.to_string());
                fails.push(format!("{name}：{msg}"));
            }
        }
        steps
            .push(json!({"step": name, "t_ms": c.elapsed(), "state": extras(c), "failed": failed}));
    };
    let w = &c.win;
    let state = |c: &Ctx| format!("{:?}", c.win.get_content_state());
    let loc_is = |c: &Ctx, p: std::path::PathBuf| c.ctl.location() == Some(p);
    let sel = |c: &Ctx| c.ctl.selected_names();

    // 1. 初始
    let names = c.ctl.names();
    step(
        c,
        "初始进入样例目录",
        vec![
            (state(c) == "Ready", "状态应为 ready"),
            (
                w.get_item_count() as usize == fixture::root_visible_count(),
                "条目数应等于可见条目数（隐藏文件不显示）",
            ),
            (
                !names.iter().any(|n| n == "隐藏的配置.ini"),
                "隐藏文件不应出现",
            ),
            (
                {
                    let mut dirs: Vec<&str> = names[..4].iter().map(String::as_str).collect();
                    dirs.sort();
                    let mut exp = [
                        fixture::DEEP[0],
                        fixture::PHOTOS,
                        fixture::EMPTY,
                        fixture::DENIED,
                    ];
                    exp.sort();
                    dirs == exp
                },
                "4 个文件夹排在文件之前",
            ),
            (
                names.iter().position(|n| n == "报告2.txt")
                    < names.iter().position(|n| n == "报告10.txt"),
                "名称为自然排序（报告2 在 报告10 前）",
            ),
            (!w.get_can_back() && w.get_can_up(), "不可后退、可上一级"),
            (w.get_selected_count() == 0, "初始无选择"),
        ],
    );

    // 2. 选择
    c.click(&c.item("图片001.webp")?);
    step(
        c,
        "单击图片001",
        vec![
            (sel(c) == ["图片001.webp"], "只选中图片001"),
            (w.get_focus_area() == "content", "焦点在内容区"),
        ],
    );
    c.shift_click(&c.item("图片004.webp")?);
    step(
        c,
        "Shift 单击图片004",
        vec![(w.get_selected_count() == 4, "范围选择 4 项")],
    );
    c.ctrl_click(&c.item("说明.md")?);
    c.ctrl_click(&c.item("图片002.webp")?);
    // 选择按显示顺序列出；名称排序与资源管理器一致（StrCmpLogicalW，中文按拼音），“说明”在“图片”前
    let expected_sel = ["说明.md", "图片001.webp", "图片003.webp", "图片004.webp"];
    step(
        c,
        "Ctrl 单击加选说明.md、取消图片002",
        vec![(sel(c) == expected_sel, "选择为 图片001/003/004 与 说明.md")],
    );

    // 3. 视图与排序不丢选择
    c.click(&c.button("列表视图")?);
    step(
        c,
        "切换到列表视图",
        vec![
            (format!("{:?}", w.get_view()) == "List", "视图为列表"),
            (sel(c) == expected_sel, "选择保持"),
            (
                w.get_item_count() as usize == fixture::root_visible_count(),
                "列表与网格条目一致",
            ),
        ],
    );
    c.click(&c.button("按大小排序")?);
    let mut sorted_sel = sel(c);
    sorted_sel.sort();
    let mut exp_sorted: Vec<String> = expected_sel.iter().map(|s| s.to_string()).collect();
    exp_sorted.sort();
    step(
        c,
        "按大小排序",
        vec![
            (format!("{:?}", w.get_sort_key()) == "Size", "排序键为大小"),
            (sorted_sel == exp_sorted, "排序后选择集合不变"),
        ],
    );
    c.click(&c.button("排序方式")?);
    c.advance(16);
    c.click(&c.button("名称")?);
    c.click(&c.button("网格视图")?);
    step(
        c,
        "排序菜单改回名称并切回网格",
        vec![
            (format!("{:?}", w.get_sort_key()) == "Name", "排序键为名称"),
            (format!("{:?}", w.get_view()) == "Grid", "视图为网格"),
            (sel(c) == expected_sel, "选择保持"),
        ],
    );
    c.with_mods(&[Key::Control], || c.key("a"));
    step(
        c,
        "Ctrl+A 全选",
        vec![(w.get_selected_count() == w.get_item_count(), "全部选中")],
    );

    // 4. 导航与历史
    c.double_click(&c.item(fixture::PHOTOS)?);
    step(
        c,
        "双击照片（读取未完成）",
        vec![
            (state(c) == "Loading", "状态为加载中"),
            (w.get_status_text() == "正在读取…", "状态栏显示正在读取"),
            (c.ctl.pending() == 1, "有 1 个待执行读取"),
        ],
    );
    c.pump();
    step(
        c,
        "照片读取完成",
        vec![
            (state(c) == "Ready" && w.get_item_count() == 12, "12 张照片"),
            (loc_is(c, fixture::path(fixture::PHOTOS)), "位置为照片"),
            (w.get_can_back(), "可后退"),
            (w.get_selected_count() == 0, "新目录无选择"),
        ],
    );
    c.click(&c.button("后退")?);
    c.pump();
    let cursor_name = |c: &Ctx| {
        usize::try_from(c.win.get_cursor_index())
            .ok()
            .and_then(|i| c.ctl.names().get(i).cloned())
    };
    step(
        c,
        "后退",
        vec![
            (loc_is(c, fixture::root()), "回到样例目录"),
            (w.get_can_forward(), "可前进"),
            (
                cursor_name(c).as_deref() == Some(fixture::PHOTOS),
                "光标落在来源文件夹“照片”",
            ),
        ],
    );
    c.click(&c.button("前进")?);
    c.pump();
    step(
        c,
        "前进",
        vec![(loc_is(c, fixture::path(fixture::PHOTOS)), "回到照片")],
    );
    c.special(Key::UpArrow, &[Key::Alt]);
    c.pump();
    step(
        c,
        "Alt+↑ 上一级",
        vec![
            (loc_is(c, fixture::root()), "回到样例目录"),
            (
                cursor_name(c).as_deref() == Some(fixture::PHOTOS),
                "光标落在“照片”",
            ),
        ],
    );

    // 5. 地址输入与各类状态
    c.click_right_edge(&c.button("地址栏")?, 12.0);
    step(
        c,
        "点击地址栏",
        vec![
            (w.get_editing_address(), "进入编辑"),
            (w.get_focus_area() == "address", "焦点在地址框"),
        ],
    );
    c.type_text(fixture::EMPTY);
    c.special(Key::Return, &[]);
    c.pump();
    step(
        c,
        "输入相对路径“空文件夹”回车",
        vec![
            (state(c) == "Empty", "状态为空目录"),
            (loc_is(c, fixture::path(fixture::EMPTY)), "位置正确"),
            (!w.get_editing_address(), "退出编辑"),
            (w.get_focus_area() == "content", "焦点回到内容区"),
        ],
    );
    c.enter_address(&fixture::path("不存在的文件夹").to_string_lossy())?;
    c.pump();
    step(
        c,
        "输入不存在的路径",
        vec![(state(c) == "NotFound", "状态为找不到")],
    );
    c.click_right_edge(&c.button("地址栏")?, 12.0);
    c.type_text("abc");
    c.special(Key::Escape, &[]);
    step(
        c,
        "编辑地址后按 Esc",
        vec![
            (!w.get_editing_address(), "取消编辑"),
            (state(c) == "NotFound", "位置不变"),
        ],
    );
    c.click(&c.button("主目录")?);
    c.pump();
    c.open_folder(fixture::DENIED)?;
    step(
        c,
        "侧栏主目录后双击“无权限”",
        vec![
            (state(c) == "AccessDenied", "状态为无权限"),
            (loc_is(c, fixture::path(fixture::DENIED)), "位置正确"),
        ],
    );
    c.click(&c.button("重试")?);
    c.pump();
    step(
        c,
        "无权限时点击重试",
        vec![(state(c) == "AccessDenied", "仍为无权限")],
    );
    c.enter_address(&fixture::path(fixture::LONG_PDF).to_string_lossy())?;
    c.pump();
    step(
        c,
        "输入长中文名 PDF 路径",
        vec![
            (state(c) == "NotAFolder", "状态为不是文件夹"),
            (c.opened.borrow().is_empty(), "不自动打开"),
        ],
    );
    c.click(&c.button("用默认应用打开")?);
    step(
        c,
        "点击“用默认应用打开”",
        vec![(
            c.opened.borrow().last() == Some(&fixture::path(fixture::LONG_PDF)),
            "交给默认应用的路径与长中文名完全一致",
        )],
    );

    // 6. Enter 打开所选文件
    c.click(&c.button("主目录")?);
    c.pump();
    c.click(&c.item(fixture::LONG_TXT)?);
    c.special(Key::Return, &[]);
    step(
        c,
        "选中长英文名文件按 Enter",
        vec![
            (c.opened.borrow().len() == 2, "打开记录 2 条"),
            (
                c.opened.borrow().last() == Some(&fixture::path(fixture::LONG_TXT)),
                "路径正确",
            ),
            (loc_is(c, fixture::root()), "位置不变"),
        ],
    );
    c.double_click(&c.item("图片007.webp")?);
    step(
        c,
        "双击图片007",
        vec![(
            c.opened.borrow().last() == Some(&fixture::path("图片007.webp")),
            "双击文件交给默认应用",
        )],
    );

    // 7. 过期结果丢弃
    c.enter_address(fixture::PHOTOS)?;
    c.enter_address(&fixture::path(fixture::EMPTY).to_string_lossy())?;
    let pending = c.ctl.pending();
    c.pump();
    step(
        c,
        "连续两次导航后一起完成读取",
        vec![
            (pending == 2, "两次读取都在排队"),
            (
                loc_is(c, fixture::path(fixture::EMPTY)) && state(c) == "Empty",
                "只应用最后一次（空文件夹）",
            ),
        ],
    );

    // 8. 深层长路径与刷新
    c.enter_address(&fixture::deep_path(fixture::DEEP.len()).to_string_lossy())?;
    c.pump();
    c.special(Key::F5, &[]);
    let refreshing = state(c) == "Loading";
    c.pump();
    step(
        c,
        "进入深层长路径后 F5 刷新",
        vec![
            (refreshing, "F5 触发重新读取"),
            (
                state(c) == "Ready" && c.ctl.names() == ["结论.txt"],
                "深层目录只有结论.txt",
            ),
        ],
    );
    Ok((steps, fails))
}
