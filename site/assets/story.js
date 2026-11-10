/* recollect — 场景页的时间线组件 */
(function () {
  "use strict";

  document.querySelectorAll("[data-story]").forEach(function (root) {
    var steps = Array.prototype.slice.call(root.querySelectorAll("[data-step]"));
    var timeEl = root.querySelector("[data-story-time]");
    var askEl = root.querySelector("[data-story-ask]");
    var outEl = root.querySelector("[data-story-out]");
    var panel = root.querySelector(".story__panel");
    var rail = root.querySelector(".story__rail");
    if (!steps.length) return;

    var ink = null;
    if (rail) {
      ink = document.createElement("span");
      ink.className = "story__ink";
      ink.setAttribute("aria-hidden", "true");
      rail.appendChild(ink);
    }

    var current = -1;

    function moveInk(step) {
      if (!ink || !step) return;
      ink.style.height = step.offsetHeight + "px";
      ink.style.transform = "translateY(" + step.offsetTop + "px)";
    }

    function select(i, force) {
      if (i === current && !force) return;
      current = i;
      steps.forEach(function (s, k) { s.classList.toggle("is-on", k === i); });
      var s = steps[i];
      timeEl.textContent = s.getAttribute("data-time") || "";
      askEl.textContent = s.getAttribute("data-ask") || "";
      outEl.textContent = (s.getAttribute("data-out") || "").replace(/\\n/g, String.fromCharCode(10));
      moveInk(s);
      if (window.RC) window.RC.swapIn(panel);
    }

    steps.forEach(function (s, i) {
      s.addEventListener("click", function () { select(i); });
    });

    window.addEventListener("resize", function () {
      if (current >= 0) moveInk(steps[current]);
    }, { passive: true });
    window.addEventListener("load", function () {
      if (current >= 0) moveInk(steps[current]);
    });

    select(0, true);
  });
})();
