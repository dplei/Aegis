// 自绘顶栏：拖动靠 data-tauri-drag-region；按钮调窗口 API；内容滚离顶部后顶栏才出现底线。
;(() => {
  const win = window.__TAURI__.window.getCurrentWindow()
  const on = (id, fn) => document.getElementById(id)?.addEventListener('click', fn)
  on('win-min', () => win.minimize())
  on('win-close', () => win.close())
  const scroll = document.getElementById('scroll')
  const bar = document.getElementById('titlebar')
  scroll?.addEventListener('scroll', () => bar.classList.toggle('scrolled', scroll.scrollTop > 0), { passive: true })
})()
