# 手势配置

Conscia 的桌面手势由输入控制器统一处理，识别方式与阈值见[手势说明](./Gestures.md)及[项目说明](../../README.md#touch--触屏)。

触控板使用二指滚动浏览小窗列，三指上下滑动切换工作区，三指张开／捏合展开／恢复分屏组，四指捏合／张开显示／退出桌面。触屏小窗列使用单指滚动，其余多指操作与触控板对应。

`input` 中的轻触点击、自然滚动和滚动方式适用于触控板输入。小窗关闭使用物理向外方向，不因自然滚动设置反转。DMS 状态栏空白区域切换工作区只接受鼠标滚轮。

`gestures` 中的 `hot-corners`、`dnd-edge-view-scroll`、`dnd-edge-workspace-switch` 保留配置解析，但不启用 Conscia 的热角或拖动边缘激活。概览通过 `toggle-overview` 动作或 DMS 工作区部件打开。
