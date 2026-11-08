// 系统文件选择对话框的自动驱动（由 scripts/dialog-baseline.ps1 用 Add-Type 编译）。
// 安全约束：只枚举并操作“指定进程号”（即本测试宿主）拥有的窗口，不枚举、不截图、不操作其他窗口。
// 做法：按控件 ID 找子窗口（文件名框 1148、过滤器下拉 1136、确定 1／取消 2／是 6／否 7），
// 向文件名框发 WM_SETTEXT（“路径＋回车”的等价做法），用 PostMessage 发 BM_CLICK 点按钮
// （确定可能弹出模态确认框，用同步 SendMessage 会被卡住）。
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class DialogDriver
{
    delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr parent, EnumProc p, IntPtr l);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr GetParent(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, string l, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll")]
    static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, IntPtr l, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);

    const uint WM_SETTEXT = 0x000C, WM_COMMAND = 0x0111, BM_CLICK = 0x00F5, CB_SETCURSEL = 0x014E, CB_GETCURSEL = 0x0147;
    const uint SMTO_ABORTIFHUNG = 2;

    public static string Cls(IntPtr h) { var s = new StringBuilder(256); GetClassName(h, s, 256); return s.ToString(); }
    public static string Text(IntPtr h) { var s = new StringBuilder(512); GetWindowText(h, s, 512); return s.ToString(); }

    /// 指定进程拥有的、可见的顶层窗口。
    public static List<IntPtr> TopWindows(uint pid)
    {
        var r = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p == pid && IsWindowVisible(h)) r.Add(h);
            return true;
        }, IntPtr.Zero);
        return r;
    }

    /// 等待宿主弹出标准对话框（类 #32770，可见）；exclude 为已知窗口（如主对话框）。返回 0 表示超时。
    public static IntPtr WaitDialog(uint pid, int timeoutMs, IntPtr exclude)
    {
        var end = Environment.TickCount + timeoutMs;
        while (Environment.TickCount < end)
        {
            foreach (var h in TopWindows(pid))
                if (h != exclude && Cls(h) == "#32770") return h;
            System.Threading.Thread.Sleep(50);
        }
        return IntPtr.Zero;
    }

    /// 按窗口类名找宿主自己的顶层窗口（带按钮窗口、所有者窗口）。
    public static IntPtr FindTop(uint pid, string cls, int timeoutMs)
    {
        var end = Environment.TickCount + timeoutMs;
        while (Environment.TickCount < end)
        {
            foreach (var h in TopWindows(pid)) if (Cls(h) == cls) return h;
            System.Threading.Thread.Sleep(50);
        }
        return IntPtr.Zero;
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, StringBuilder l, uint flags, uint timeout, out IntPtr result);

    /// 读取另一进程里 Edit 控件的全文（WM_GETTEXT 由系统跨进程搬运字符串）。
    public static string EditText(IntPtr h)
    {
        var sb = new StringBuilder(65536);
        IntPtr res;
        SendMessageTimeout(h, 0x000D /*WM_GETTEXT*/, (IntPtr)65535, sb, SMTO_ABORTIFHUNG, 3000, out res);
        return sb.ToString();
    }

    public static bool Alive(IntPtr h) { return IsWindow(h) && IsWindowVisible(h); }

    /// 在对话框的全部后代里按控件 ID（及可选的类名）找子窗口；先找到的优先，ComboBoxEx32 内部的 Edit 用 cls="Edit" 精确取到。
    public static IntPtr Child(IntPtr dlg, int id, string cls)
    {
        IntPtr found = IntPtr.Zero;
        EnumChildWindows(dlg, (h, l) => {
            if (GetDlgCtrlID(h) == id && (cls == null || Cls(h) == cls)) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    /// 文件名／路径输入框：打开类对话框是 ID 1148 的 Edit；Vista 风格另存为是 ComboBox 里 ID 1001 的 Edit；
    /// SHBrowseForFolder 的编辑框 ID 为 0x3744。不会取到地址栏（ID 41477）与搜索框。
    public static IntPtr FileNameEdit(IntPtr dlg)
    {
        var e = Child(dlg, 1148, "Edit");
        if (e == IntPtr.Zero) e = Child(dlg, 1152, "Edit"); // 选择文件夹（FOS_PICKFOLDERS）
        if (e != IntPtr.Zero) return e;
        e = Child(dlg, 0x3744, "Edit");
        if (e != IntPtr.Zero) return e;
        IntPtr found = IntPtr.Zero;
        EnumChildWindows(dlg, (h, l) => {
            if (GetDlgCtrlID(h) == 1001 && Cls(h) == "Edit" && Cls(GetParent(h)) == "ComboBox") { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    /// 点“覆盖确认”这类消息框的“是／否”：先按标准 ID（6／7），没有时按出现顺序（是在前、否在后；文字随系统语言变化，不依赖）。
    public static string ClickConfirm(IntPtr box, bool yes)
    {
        int id = yes ? 6 : 7;
        var b = Child(box, id, "Button");
        string how = "id";
        if (b == IntPtr.Zero)
        {
            var buttons = new List<IntPtr>();
            EnumChildWindows(box, (h, l) => { if (Cls(h) == "Button") buttons.Add(h); return true; }, IntPtr.Zero);
            if (buttons.Count < 2) return null;
            b = buttons[yes ? 0 : 1];
            how = "order";
        }
        PostMessage(b, BM_CLICK, IntPtr.Zero, IntPtr.Zero);
        return how + ":" + Text(b);
    }

    public static string Dump(IntPtr dlg)
    {
        var sb = new StringBuilder();
        sb.AppendLine("window " + dlg + " class=" + Cls(dlg) + " title=" + Text(dlg));
        EnumChildWindows(dlg, (h, l) => {
            sb.AppendLine("  child " + h + " id=" + GetDlgCtrlID(h) + " class=" + Cls(h) + " text=" + Text(h));
            return true;
        }, IntPtr.Zero);
        return sb.ToString();
    }

    public static bool SetText(IntPtr h, string text)
    {
        IntPtr res;
        return SendMessageTimeout(h, WM_SETTEXT, IntPtr.Zero, text, SMTO_ABORTIFHUNG, 3000, out res) != IntPtr.Zero;
    }

    public static bool Click(IntPtr dlg, int id)
    {
        var b = Child(dlg, id, "Button");
        if (b == IntPtr.Zero) b = Child(dlg, id, null);
        if (b == IntPtr.Zero) return false;
        return PostMessage(b, BM_CLICK, IntPtr.Zero, IntPtr.Zero);
    }

    /// 选择下拉框第 index0 项（从 0 起）：给下拉框投递方向键，让控件自己改选并通知父窗口
    /// （直接 CB_SETCURSEL 不会让对话框更新“当前过滤器”）。
    public static IntPtr FindFilterCombo(IntPtr dlg)
    {
        // 打开类对话框的文件类型下拉框 ID 为 1136；Vista 风格另存为里是没有 ID、也没有内部 Edit 的那个 ComboBox
        var c = Child(dlg, 1136, "ComboBox");
        if (c == IntPtr.Zero)
        {
            EnumChildWindows(dlg, (h, l) => {
                if (Cls(h) == "ComboBox" && GetDlgCtrlID(h) == 0 && !HasChildClass(h, "Edit")) { c = h; return false; }
                return true;
            }, IntPtr.Zero);
        }
        return c;
    }

    public static bool SelectFilter(IntPtr dlg, int index0)
    {
        var c = FindFilterCombo(dlg);
        if (c == IntPtr.Zero) return false;
        return SelectCombo(c, index0);
    }

    static bool HasChildClass(IntPtr parent, string cls)
    {
        bool has = false;
        EnumChildWindows(parent, (h, l) => { if (Cls(h) == cls) { has = true; return false; } return true; }, IntPtr.Zero);
        return has;
    }

    public static string ComboState(IntPtr c)
    {
        IntPtr cur, cnt;
        SendMessageTimeout(c, CB_GETCURSEL, IntPtr.Zero, IntPtr.Zero, SMTO_ABORTIFHUNG, 3000, out cur);
        SendMessageTimeout(c, 0x0146 /*CB_GETCOUNT*/, IntPtr.Zero, IntPtr.Zero, SMTO_ABORTIFHUNG, 3000, out cnt);
        return "cursel=" + (int)cur + " count=" + (int)cnt + " parent=" + Cls(GetParent(c));
    }

    public static bool SelectCombo(IntPtr c, int index0)
    {
        IntPtr cur;
        for (int i = 0; i < 64; i++)
        {
            SendMessageTimeout(c, CB_GETCURSEL, IntPtr.Zero, IntPtr.Zero, SMTO_ABORTIFHUNG, 3000, out cur);
            int now = (int)cur;
            if (now == index0) return true;
            uint vk = now < index0 ? 0x28u /*VK_DOWN*/ : 0x26u /*VK_UP*/;
            PostMessage(c, 0x0100 /*WM_KEYDOWN*/, (IntPtr)vk, (IntPtr)1);
            PostMessage(c, 0x0101 /*WM_KEYUP*/, (IntPtr)vk, unchecked((IntPtr)(int)0xC0000001));
            System.Threading.Thread.Sleep(120);
        }
        return false;
    }
}
