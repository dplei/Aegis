// 首屏前决定深浅色，避免闪烁；切换那一帧关掉过渡。
;(() => {
  const root = document.documentElement
  const mq = matchMedia('(prefers-color-scheme: dark)')
  const saved = () => { try { return localStorage.getItem('theme') } catch { return null } }
  const apply = (dark) => {
    root.classList.add('theming')
    root.classList.toggle('dark', dark)
    void root.offsetWidth
    requestAnimationFrame(() => root.classList.remove('theming'))
  }
  root.classList.toggle('dark', saved() ? saved() === 'dark' : mq.matches)
  mq.addEventListener('change', (e) => { if (!saved()) apply(e.matches) })
  window.toggleTheme = () => {
    const dark = !root.classList.contains('dark')
    apply(dark)
    try { localStorage.setItem('theme', dark ? 'dark' : 'light') } catch {}
  }
})()
