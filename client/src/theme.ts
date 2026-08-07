// 主题应用工具。
//
// 把用户选择的主题写入 <html data-theme="...">，CSS 据此切换明暗，
// 而不再依赖系统级 `prefers-color-scheme`（那样用户选择不会生效）。

export type Theme = 'System' | 'Light' | 'Dark';

/** 把主题应用到 document.documentElement。System 跟随系统偏好。 */
export function applyTheme(theme: Theme): void {
  document.documentElement.setAttribute('data-theme', theme.toLowerCase());
}
