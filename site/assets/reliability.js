/* recollect — reliability simulator: 断网、自蒸、回并 */
(function () {
  "use strict";

  var root = document.querySelector("[data-sim]");
  if (!root) return;

  var $ = function (sel) { return root.querySelector(sel); };
  var clockEl = $("[data-clock]");
  var stateEl = $("[data-state]");
  var fillEl = $("[data-fill]");
  var headEl = $("[data-head]");
  var badgeEl = $("[data-badge]");
  var mark24 = $("[data-mark24]");
  var lbl24 = $("[data-lbl24]");
  var mark120 = $("[data-mark120]");
  var lbl120 = $("[data-lbl120]");
  var rSelf = $("[data-r-self]");
  var rPending = $("[data-r-pending]");
  var rStaging = $("[data-r-staging]");
  var rDr = $("[data-r-dr]");
  var nodeBox = $("[data-node-box]");
  var hubBox = $("[data-hub-box]");
  var stEls = document.querySelectorAll("[data-st]");
  var hintEl = $("[data-simhint]");
  var ordEls = document.querySelectorAll("[data-ord]");
  var btns = {
    cut: root.querySelector('[data-act="cut"]'),
    h12: root.querySelector('[data-act="12"]'),
    h24: root.querySelector('[data-act="24"]'),
    h120: root.querySelector('[data-act="120"]'),
    rejoin: root.querySelector('[data-act="rejoin"]'),
    reset: root.querySelector('[data-act="reset"]')
  };

  var sim = { cut: false, hours: 0, rejoining: false, restored: false };
  var pendingState = 0;
  var STATE_LABEL = {
    connected: "正常同步",
    unreachable: "连不上中心",
    offline_distilling: "本地整理中",
    detached_distilling: "脱离中心 · 整理中",
    detached_promoted: "自己当中心",
    rejoined: "已并回"
  };
  var reduce = window.RC ? window.RC.reduceMotion : false;
  var shownHours = 0;
  var tweenRaf = 0;

  function fmt(n) { return n.toLocaleString("en-US"); }

  function setVal(el, text, cls, quiet) {
    if (!el) return;
    if (el.textContent !== text) {
      el.textContent = text;
      if (!quiet) {
        el.classList.remove("flash");
        void el.offsetWidth;
        el.classList.add("flash");
      }
    }
    if (cls !== undefined) el.className = "readout__v " + cls;
  }

  function paintNumbers(h) {
    clockEl.textContent = fmt(Math.round(h));
    var pending = h >= 24 ? Math.round((h - 24) * 2.5) : 0;
    var staging = pending * 4;
    var dr = h >= 120 ? Math.floor((h - 120) / 12) + 1 : 0;
    var selfOn = h >= 24 && !sim.restored;
    setVal(rSelf, selfOn ? "true" : "false", selfOn ? "is-brass" : "", true);
    setVal(rPending, fmt(pending), pending > 0 ? "is-warn" : "", true);
    setVal(rStaging, fmt(staging), staging > 0 ? "is-warn" : "", true);
    setVal(rDr, fmt(dr), dr > 0 ? "is-brass" : "", true);
  }

  function tweenNumbers(to) {
    cancelAnimationFrame(tweenRaf);
    if (reduce || Math.abs(to - shownHours) < 0.001) {
      shownHours = to;
      paintNumbers(to);
      return;
    }
    var from = shownHours, start = 0, dur = 520;
    function step(ts) {
      if (!start) start = ts;
      var p = Math.min((ts - start) / dur, 1);
      var e = 1 - Math.pow(1 - p, 3);
      shownHours = from + (to - from) * e;
      paintNumbers(shownHours);
      if (p < 1) tweenRaf = requestAnimationFrame(step);
      else { shownHours = to; paintNumbers(to); }
    }
    tweenRaf = requestAnimationFrame(step);
  }

  function currentState() {
    if (sim.restored) return "rejoined";
    if (!sim.cut) return "connected";
    if (sim.hours < 24) return "unreachable";
    if (sim.hours < 120) return "offline_distilling";
    return "detached_promoted";
  }

  function render(animate) {
    var state = currentState();
    var pct = Math.min(sim.hours / 120, 1) * 100;

    if (animate) {
      tweenNumbers(sim.hours);
    } else {
      cancelAnimationFrame(tweenRaf);
      shownHours = sim.hours;
      paintNumbers(sim.hours);
    }
    fillEl.style.width = pct + "%";
    headEl.style.left = pct + "%";

    stateEl.textContent = STATE_LABEL[state] || state;
    stateEl.style.color = state === "connected" ? "var(--sage)" : state === "rejoined" ? "var(--sage)" : "var(--brass)";

    badgeEl.textContent = STATE_LABEL[state] || state;
    badgeEl.className = "chip " +
      (state === "connected" || state === "rejoined" ? "chip--sage" : "chip--brass");

    if (hintEl) {
      var hint;
      if (sim.rejoining) hint = "回并中：推上去 → 送回去 → 确认 → 拉快照";
      else if (sim.restored) hint = "已恢复；点「重置」可以再演一遍";
      else if (!sim.cut) hint = "第一步：先断开和中心的连接";
      else if (sim.hours === 0) hint = "已断开。推进时间，看 24h 与 120h 两道关";
      else if (sim.hours < 24) hint = "还没到一天，继续推进";
      else if (sim.hours < 120) hint = "已进入本地整理；推到 120h，看它自己当中心";
      else hint = "满五天了：可以恢复连接，看回并四步";
      hintEl.textContent = hint;
    }

    root.classList.toggle("is-part", sim.cut && !sim.restored);
    nodeBox.classList.toggle("is-part", sim.cut && !sim.restored);
    hubBox.classList.toggle("is-part", sim.cut && !sim.restored);

    mark24.classList.toggle("is-on", sim.hours >= 24);
    lbl24.classList.toggle("is-on", sim.hours >= 24);
    mark120.classList.toggle("is-on", sim.hours >= 120);
    lbl120.classList.toggle("is-on", sim.hours >= 120);

    stEls.forEach(function (el) {
      var k = el.getAttribute("data-st");
      var on =
        (k === "connected" && (state === "connected" || state === "unreachable")) ||
        (k === "offline" && state === "offline_distilling") ||
        (k === "detached_promoted" && state === "detached_promoted") ||
        (k === "rejoined" && state === "rejoined");
      el.classList.toggle("is-on", on);
    });

    btns.cut.disabled = sim.cut || sim.rejoining;
    btns.h12.disabled = !sim.cut || sim.rejoining;
    btns.h24.disabled = !sim.cut || sim.rejoining;
    btns.h120.disabled = !sim.cut || sim.rejoining;
    btns.rejoin.disabled = !sim.cut || sim.rejoining;
  }

  function advance(h) {
    if (!sim.cut) return;
    sim.hours = Math.min(sim.hours + h, 168);
    render(true);

    if (sim.hours >= 120) {
      var target = 120;
      /* 到五天闸时先展示 detached_distilling，再转 promoted */
      if (sim.hours >= target && !pendingState) {
        pendingState = 1;
        stateEl.textContent = STATE_LABEL.detached_distilling;
        setTimeout(function () {
          pendingState = 0;
          render(false);
        }, 1100);
      }
    }
  }

  function rejoin() {
    if (!sim.cut || sim.rejoining) return;
    sim.rejoining = true;
    sim.restored = false;
    render(false);

    var steps = Array.prototype.slice.call(ordEls);
    steps.forEach(function (s) { s.classList.remove("is-on", "is-done"); });

    var i = 0;
    function next() {
      if (i > 0) steps[i - 1].classList.replace("is-on", "is-done");
      if (i >= steps.length) {
        sim.cut = false;
        sim.rejoining = false;
        sim.restored = true;
        sim.hours = 0;
        render(false);
        steps.forEach(function (s) { s.classList.remove("is-on"); s.classList.add("is-done"); });
        return;
      }
      steps[i].classList.add("is-on");
      i++;
      setTimeout(next, 900);
    }
    next();
  }

  btns.cut.addEventListener("click", function () {
    btns.cut.classList.remove("ctl--cue");
    sim.cut = true;
    sim.restored = false;
    sim.hours = 0;
    render(false);
  });
  btns.h12.addEventListener("click", function () { advance(12); });
  btns.h24.addEventListener("click", function () {
    if (!sim.cut) return;
    sim.hours = 24;
    render(true);
  });
  btns.h120.addEventListener("click", function () {
    if (!sim.cut) return;
    sim.hours = 120;
    render(true);
    stateEl.textContent = STATE_LABEL.detached_distilling;
    setTimeout(function () { render(false); }, 1100);
  });
  btns.rejoin.addEventListener("click", rejoin);
  btns.reset.addEventListener("click", function () {
    sim = { cut: false, hours: 0, rejoining: false, restored: false };
    pendingState = 0;
    ordEls.forEach(function (s) { s.classList.remove("is-on", "is-done"); });
    render(false);
  });

  render(false);
})();
