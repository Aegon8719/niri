# 最大化、分屏组与全屏

| 操作 | 默认按键／手势 | 行为 |
| --- | --- | --- |
| `maximize-window-to-edges` | Mod+F、Mod+M | 当前应用单独最大化，再次恢复 |
| `maximize-column` | Mod+Ctrl+F | 整个分屏组占满可用工作区，再次恢复 |
| 整组展开／恢复 | 触屏、触控板三指张开／捏合 | 保留组内分割与比例 |
| `fullscreen-window` | Mod+Shift+F | 当前应用显式全屏／恢复 |
| `toggle-show-desktop` | Super+D；四指捏合／张开 | 隐藏／恢复全部应用窗口，保留布局 |

仅有一个应用的工作区自动填满主区域，不允许将该应用最小化到小窗列或最大化。全屏不受此限制。

最大化保留面板空间、焦点高亮、圆角及边框留白。整组展开保留分屏树；单应用最大化暂时隐藏其他窗口，恢复后继续使用原布局。全屏独立于最大化，使用全屏装饰和层级规则。

`toggle-windowed-fullscreen` 仅向客户端声明全屏，以隐藏客户端界面；窗口仍由布局分配尺寸。窗口初始状态可通过 `open-maximized`、`open-maximized-to-edges` 和 `open-fullscreen` 规则控制，单应用限制仍由布局处理。

完整快捷键见[项目说明](../../README.md#keyboard-shortcuts--键盘快捷键)。
