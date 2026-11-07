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
