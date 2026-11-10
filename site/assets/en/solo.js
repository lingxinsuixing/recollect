/* recollect — solo page: 单机 / 两台设备切换 */
(function () {
  "use strict";

  var root = document.querySelector("[data-solomode]");
  if (!root) return;

  var btns = Array.prototype.slice.call(root.querySelectorAll("[data-solo-mode]"));
  var panels = Array.prototype.slice.call(root.querySelectorAll("[data-solo-panel]"));

  function select(mode, animate) {
    btns.forEach(function (b) {
      var on = b.getAttribute("data-solo-mode") === mode;
      b.classList.toggle("is-on", on);
      b.setAttribute("aria-pressed", on ? "true" : "false");
    });
    panels.forEach(function (p) {
      var on = p.getAttribute("data-solo-panel") === mode;
      p.hidden = !on;
      if (on && animate && window.RC) window.RC.swapIn(p);
    });
  }

  btns.forEach(function (b) {
    b.addEventListener("click", function () {
      select(b.getAttribute("data-solo-mode"), true);
    });
  });

  select("one", false);
})();
