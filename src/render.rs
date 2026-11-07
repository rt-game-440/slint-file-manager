//! 渲染器选择与原生窗口属性（Issue #39）。
//!
//! - **根因修复**：Slint 的 winit 后端默认给每个窗口 `with_transparent(true)`；在 Windows 上 winit 因此调用
//!   `DwmEnableBlurBehindWindow`（空模糊区域），桌面合成按窗口内容的 alpha 通道混合。开发机（NVIDIA 独显 +
//!   AMD 核显 + 远程控制软件的虚拟主显示器）上 femtovg／OpenGL 交给 DWM 的画面 alpha 为 0，整个无边框窗口
//!   在合成结果里完全透明。本应用窗口背景不透明、弹出层也不是独立窗口，不需要透明窗口，所以对所有窗口
//!   改为 `with_transparent(false)`。诊断数据见 `docs/adr/0010-opaque-window-renderer-switch.md`。
//! - **手动开关**（排查或兼容极端环境）：命令行 `--renderer=software|gl` > 环境变量 `SFM_RENDERER` >
//!   Slint 自带的 `SLINT_BACKEND`（如 `winit-software`）> 默认。默认不指定渲染器名，沿用 Slint 的选择顺序：
//!   femtovg（OpenGL），初始化失败时回退软件渲染。

use slint::winit_030::winit::window::WindowAttributes;

/// 手动指定的渲染器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    /// femtovg（OpenGL），Slint 名称 `femtovg`。
    Gl,
    /// CPU 软件渲染，Slint 名称 `software`。
    Software,
}

impl Renderer {
    fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "gl" | "opengl" | "femtovg" => Some(Self::Gl),
            "software" | "sw" | "cpu" => Some(Self::Software),
            _ => None,
        }
    }

    fn slint_name(self) -> &'static str {
        match self {
            Self::Gl => "femtovg",
            Self::Software => "software",
        }
    }
}

/// 选择结果：显式指定的渲染器（`None` = 交给 `SLINT_BACKEND` 或 Slint 默认顺序）与来源说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub renderer: Option<Renderer>,
    pub source: &'static str,
}

/// 纯函数：按优先级决定渲染器。无法识别的取值记入 `Err`（调用方提示后按默认继续）。
pub fn choose(args: &[String], sfm_renderer: Option<&str>) -> Result<Choice, String> {
    if let Some(v) = args.iter().find_map(|a| a.strip_prefix("--renderer=")) {
        return Renderer::parse(v)
            .map(|r| Choice {
                renderer: Some(r),
                source: "--renderer",
            })
            .ok_or_else(|| format!("无法识别的 --renderer={v}（可用 gl、software）"));
    }
    if let Some(v) = sfm_renderer.filter(|v| !v.trim().is_empty()) {
        return Renderer::parse(v)
            .map(|r| Choice {
                renderer: Some(r),
                source: "SFM_RENDERER",
            })
            .ok_or_else(|| format!("无法识别的 SFM_RENDERER={v}（可用 gl、software）"));
    }
    Ok(Choice {
        renderer: None,
        source: "默认",
    })
}

/// 所有原生窗口统一关闭透明（见模块说明）。
pub fn window_attributes(attrs: WindowAttributes) -> WindowAttributes {
    attrs.with_transparent(false)
}

/// 在创建任何 Slint 窗口之前调用：选定 winit 后端与渲染器，并挂上窗口属性钩子。
pub fn select(args: &[String]) -> Result<Choice, slint::PlatformError> {
    let choice = choose(args, std::env::var("SFM_RENDERER").ok().as_deref()).unwrap_or_else(|e| {
        eprintln!("{e}，改用默认渲染器");
        Choice {
            renderer: None,
            source: "默认",
        }
    });
    let mut selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(window_attributes);
    if let Some(r) = choice.renderer {
        selector = selector.renderer_name(r.slint_name().into());
    }
    selector.select()?;
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_leaves_renderer_to_slint() {
        let c = choose(&args(&["exe"]), None).unwrap();
        assert_eq!(c.renderer, None);
        let c = choose(&args(&["exe"]), Some("  ")).unwrap();
        assert_eq!(c.renderer, None);
    }

    #[test]
    fn command_line_wins_over_env() {
        let c = choose(&args(&["exe", "--renderer=software"]), Some("gl")).unwrap();
        assert_eq!(c.renderer, Some(Renderer::Software));
        assert_eq!(c.source, "--renderer");
        let c = choose(&args(&["exe", "--renderer=GL"]), Some("software")).unwrap();
        assert_eq!(c.renderer, Some(Renderer::Gl));
    }

    #[test]
    fn env_used_when_no_flag() {
        let c = choose(&args(&["exe"]), Some("sw")).unwrap();
        assert_eq!(c.renderer, Some(Renderer::Software));
        assert_eq!(c.source, "SFM_RENDERER");
    }

    #[test]
    fn unknown_values_are_errors() {
        assert!(choose(&args(&["exe", "--renderer=skia"]), None).is_err());
        assert!(choose(&args(&["exe"]), Some("vulkan")).is_err());
    }

    #[test]
    fn hook_turns_transparency_off() {
        let a = window_attributes(WindowAttributes::default().with_transparent(true));
        assert!(!a.transparent);
    }
}
