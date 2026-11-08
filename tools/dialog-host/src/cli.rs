//! 命令行解析：`方式 + 场景 + 选项` 描述一次对话框调用（请求），GUI 按钮与命令行共用这一份。

use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Api {
    /// 现代 `IFileOpenDialog`／`IFileSaveDialog`
    Ifd,
    /// 旧式 `GetOpenFileNameW`／`GetSaveFileNameW`
    Gofn,
    /// 旧式文件夹选择 `SHBrowseForFolderW`
    Sbf,
}

impl Api {
    pub fn parse(s: &str) -> Result<Api, String> {
        match s {
            "ifd" => Ok(Api::Ifd),
            "gofn" => Ok(Api::Gofn),
            "sbf" => Ok(Api::Sbf),
            _ => Err(format!("未知方式 {s:?}，可选 ifd、gofn、sbf")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Api::Ifd => "ifd",
            Api::Gofn => "gofn",
            Api::Sbf => "sbf",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scenario {
    OpenSingle,
    OpenMulti,
    Save,
    PickFolder,
}

impl Scenario {
    pub fn parse(s: &str) -> Result<Scenario, String> {
        match s {
            "open-single" => Ok(Scenario::OpenSingle),
            "open-multi" => Ok(Scenario::OpenMulti),
            "save" => Ok(Scenario::Save),
            "pick-folder" => Ok(Scenario::PickFolder),
            _ => Err(format!(
                "未知场景 {s:?}，可选 open-single、open-multi、save、pick-folder"
            )),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Scenario::OpenSingle => "open-single",
            Scenario::OpenMulti => "open-multi",
            Scenario::Save => "save",
            Scenario::PickFolder => "pick-folder",
        }
    }
}

/// 文件类型过滤器：显示名 + 以分号分隔的通配符，如 `文本文件|*.txt;*.md`。
#[derive(Clone, Debug)]
pub struct Filter {
    pub name: String,
    pub patterns: String,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub api: Api,
    pub scenario: Scenario,
    pub init_dir: Option<String>,
    pub title: Option<String>,
    pub filters: Vec<Filter>,
    /// 初始选中的过滤器，从 1 起
    pub filter_index: Option<u32>,
    pub default_name: Option<String>,
    pub default_ext: Option<String>,
    /// 保存时同名文件是否要求覆盖确认（ifd：FOS_OVERWRITEPROMPT；gofn：OFN_OVERWRITEPROMPT）
    pub overwrite_prompt: bool,
    /// sbf 样式：`new`（BIF_NEWDIALOGSTYLE|BIF_EDITBOX）或 `legacy`（只有目录树）
    pub sbf_style: String,
    /// 所有者窗口：`none` 或 `window`
    pub owner: String,
}

/// 与请求无关的运行选项。
#[derive(Default, Clone, Debug)]
pub struct Common {
    pub out: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
}

impl Request {
    pub fn to_json(&self) -> Value {
        json!({
            "api": self.api.name(),
            "scenario": self.scenario.name(),
            "init_dir": self.init_dir,
            "title": self.title,
            "filters": self.filters.iter().map(|f| json!({"name": f.name, "patterns": f.patterns})).collect::<Vec<_>>(),
            "filter_index": self.filter_index,
            "default_name": self.default_name,
            "default_ext": self.default_ext,
            "overwrite_prompt": self.overwrite_prompt,
            "sbf_style": self.sbf_style,
            "owner": self.owner,
        })
    }

    pub fn parse(api: &str, scenario: &str, opts: &[String]) -> Result<(Request, Common), String> {
        let api = Api::parse(api)?;
        let scenario = Scenario::parse(scenario)?;
        let ok = match api {
            Api::Ifd => true,
            Api::Gofn => scenario != Scenario::PickFolder,
            Api::Sbf => scenario == Scenario::PickFolder,
        };
        if !ok {
            return Err(format!(
                "方式 {} 不支持场景 {}（gofn 没有选文件夹，sbf 只能选文件夹）",
                api.name(),
                scenario.name()
            ));
        }
        let mut r = Request {
            api,
            scenario,
            init_dir: None,
            title: None,
            filters: Vec::new(),
            filter_index: None,
            default_name: None,
            default_ext: None,
            overwrite_prompt: true,
            sbf_style: "new".into(),
            owner: "none".into(),
        };
        let mut c = Common::default();
        for o in opts {
            let (k, v) = o
                .strip_prefix("--")
                .and_then(|s| s.split_once('='))
                .ok_or_else(|| format!("选项格式应为 --名称=值：{o}"))?;
            match k {
                "init-dir" => r.init_dir = Some(v.to_string()),
                "title" => r.title = Some(v.to_string()),
                "filter" => {
                    let (name, pats) = match v.split_once('|') {
                        Some((n, p)) => (n.to_string(), p.to_string()),
                        None => (v.to_string(), v.to_string()),
                    };
                    if pats.is_empty() {
                        return Err(format!("过滤器缺少通配符：{v}"));
                    }
                    r.filters.push(Filter {
                        name,
                        patterns: pats,
                    });
                }
                "filter-index" => {
                    let n: u32 = v
                        .parse()
                        .map_err(|_| format!("filter-index 不是数字：{v}"))?;
                    if n == 0 {
                        return Err("filter-index 从 1 起".into());
                    }
                    r.filter_index = Some(n);
                }
                "default-name" => r.default_name = Some(v.to_string()),
                "default-ext" => r.default_ext = Some(v.trim_start_matches('.').to_string()),
                "overwrite-prompt" => {
                    r.overwrite_prompt = match v {
                        "on" => true,
                        "off" => false,
                        _ => return Err("overwrite-prompt 取 on 或 off".into()),
                    }
                }
                "sbf-style" => match v {
                    "new" | "legacy" => r.sbf_style = v.to_string(),
                    _ => return Err("sbf-style 取 new 或 legacy".into()),
                },
                "owner" => match v {
                    "none" | "window" => r.owner = v.to_string(),
                    _ => return Err("owner 取 none 或 window".into()),
                },
                "out" => c.out = Some(PathBuf::from(v)),
                "log-file" => c.log_file = Some(PathBuf::from(v)),
                "log-dir" => c.log_dir = Some(PathBuf::from(v)),
                _ => return Err(format!("未知选项 --{k}")),
            }
        }
        if let Some(n) = r.filter_index
            && n as usize > r.filters.len()
        {
            return Err(format!(
                "filter-index={n} 超出过滤器数量 {}",
                r.filters.len()
            ));
        }
        Ok((r, c))
    }
}
