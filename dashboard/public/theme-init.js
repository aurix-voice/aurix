// Applies the stored theme before the first paint so a dark-mode reload does not flash white.
// Kept as a separate file (not inline) so the Content-Security-Policy can stay `script-src 'self'`.
(function () {
  try {
    var t = localStorage.getItem("aurix.theme");
    var dark = t === "dark" || (t !== "light" && matchMedia("(prefers-color-scheme: dark)").matches);
    if (dark) document.documentElement.classList.add("dark");
  } catch (e) {
    /* storage disabled */
  }
})();
