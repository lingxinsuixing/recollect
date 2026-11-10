/* recollect — shared site behaviour */
(function () {
  "use strict";

  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* --- 导航：当前页高亮 ------------------------------------------------ */
  function initNav() {
    var here = location.pathname.split("/").pop() || "index.html";
    var active = null;
    document.querySelectorAll("[data-nav-link]").forEach(function (a) {
      var href = a.getAttribute("href");
      if (href === here || (here === "" && href === "index.html")) {
        a.setAttribute("aria-current", "page");
        active = a;
      }
    });
    if (active && window.matchMedia("(max-width: 760px)").matches) {
      active.scrollIntoView({ block: "nearest", inline: "center" });
    }
  }

  /* --- 移动端导航：还有没露出来的页就淡出右边缘 ------------------------- */
  function initNavMore() {
    var links = document.querySelector(".nav__links");
    if (!links) return;
    function update() {
      var more = links.scrollLeft + links.clientWidth < links.scrollWidth - 2;
      var back = links.scrollLeft > 2;
      links.classList.toggle("is-more", more);
      links.classList.toggle("is-back", back);
    }
    links.addEventListener("scroll", update, { passive: true });
    window.addEventListener("resize", update, { passive: true });
    update();
  }

  /* --- 滚动进度 -------------------------------------------------------- */
  function initProgress() {
    var bar = document.querySelector("[data-progress]");
    if (!bar) return;
    var ticking = false;
    function update() {
      var h = document.documentElement;
      var max = h.scrollHeight - h.clientHeight;
      var p = max > 0 ? h.scrollTop / max : 0;
      bar.style.width = (p * 100).toFixed(2) + "%";
      ticking = false;
    }
    window.addEventListener("scroll", function () {
      if (!ticking) {
        ticking = true;
        requestAnimationFrame(update);
      }
    }, { passive: true });
    update();
  }

  /* --- 揭示 ------------------------------------------------------------ */
  function initReveal() {
    var items = document.querySelectorAll("[data-reveal]");
    if (!items.length) return;

    if (reduceMotion || !("IntersectionObserver" in window)) {
      items.forEach(function (el) { el.classList.add("is-in"); });
      return;
    }

    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add("is-in");
          io.unobserve(e.target);
        }
      });
    }, { rootMargin: "0px 0px -8% 0px", threshold: 0.08 });

    items.forEach(function (el) { io.observe(el); });

    /* 快速跳转/锚点会跳过某些元素，补一个越屏检查 */
    var pending = Array.prototype.slice.call(items);
    var tick = false;
    function sweep() {
      tick = false;
      if (!pending.length) return;
      var limit = window.innerHeight * 0.94;
      pending = pending.filter(function (el) {
        if (el.classList.contains("is-in")) return false;
        if (el.getBoundingClientRect().top < limit) {
          el.classList.add("is-in");
          io.unobserve(el);
          return false;
        }
        return true;
      });
    }
    window.addEventListener("scroll", function () {
      if (!tick) { tick = true; requestAnimationFrame(sweep); }
    }, { passive: true });
    window.addEventListener("resize", sweep, { passive: true });
    sweep();
  }

  /* --- 数字滚动 -------------------------------------------------------- */
  var easeOut = function (t) { return 1 - Math.pow(1 - t, 3); };

  function animateNumber(el) {
    var raw = el.getAttribute("data-count");
    var target = parseFloat(raw);
    var decimals = (raw.split(".")[1] || "").length;
    var suffix = el.getAttribute("data-suffix") || "";
    var prefix = el.getAttribute("data-prefix") || "";
    if (isNaN(target)) return;

    if (reduceMotion) {
      el.textContent = prefix +
        (decimals ? target.toFixed(decimals) : Math.round(target).toLocaleString("en-US")) +
        suffix;
      return;
    }

    var dur = 1500;
    var start = null;
    function step(ts) {
      if (!start) start = ts;
      var p = Math.min((ts - start) / dur, 1);
      var v = target * easeOut(p);
      el.textContent =
        prefix +
        (decimals ? v.toFixed(decimals) : Math.round(v).toLocaleString("en-US")) +
        suffix;
      if (p < 1) requestAnimationFrame(step);
    }
    requestAnimationFrame(step);
  }

  function initCounters() {
    var els = document.querySelectorAll("[data-count]");
    if (!els.length) return;
    if (!("IntersectionObserver" in window)) {
      els.forEach(animateNumber);
      return;
    }
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          animateNumber(e.target);
          io.unobserve(e.target);
        }
      });
    }, { threshold: 0.4 });
    els.forEach(function (el) { io.observe(el); });
  }

  /* --- 通用 tab -------------------------------------------------------- */
  function initTabs() {
    document.querySelectorAll("[data-tabs]").forEach(function (root) {
      var tabs = Array.from(root.querySelectorAll("[role='tab']"));
      var panels = Array.from(root.querySelectorAll("[role='tabpanel']"));
      if (!tabs.length) return;

      var bar = root.querySelector(".tabs");
      var ink = null;
      if (bar) {
        ink = document.createElement("span");
        ink.className = "tabs__ink";
        ink.setAttribute("aria-hidden", "true");
        bar.appendChild(ink);
      }

      function moveInk(t) {
        if (!ink || !t) return;
        ink.style.width = t.offsetWidth + "px";
        ink.style.transform =
          "translate(" + t.offsetLeft + "px," + (t.offsetTop + t.offsetHeight - 1) + "px)";
      }

      function select(idx) {
        tabs.forEach(function (t, i) {
          var on = i === idx;
          t.setAttribute("aria-selected", on ? "true" : "false");
          t.tabIndex = on ? 0 : -1;
        });
        moveInk(tabs[idx]);
        panels.forEach(function (p, i) {
          p.hidden = i !== idx;
          if (i === idx) {
            p.classList.remove("is-entering");
            void p.offsetWidth;
            p.classList.add("is-entering");
          }
        });
        root.dispatchEvent(new CustomEvent("tabchange", { detail: { index: idx } }));
      }

      tabs.forEach(function (t, i) {
        t.addEventListener("click", function () { select(i); });
        t.addEventListener("keydown", function (ev) {
          var n = null;
          if (ev.key === "ArrowRight") n = (i + 1) % tabs.length;
          if (ev.key === "ArrowLeft") n = (i - 1 + tabs.length) % tabs.length;
          if (ev.key === "Home") n = 0;
          if (ev.key === "End") n = tabs.length - 1;
          if (n !== null) {
            ev.preventDefault();
            tabs[n].focus();
            select(n);
          }
        });
      });

      window.addEventListener("resize", function () {
        var cur = tabs.filter(function (t) { return t.getAttribute("aria-selected") === "true"; })[0];
        moveInk(cur);
      }, { passive: true });

      select(0);
      window.addEventListener("load", function () {
        var cur = tabs.filter(function (t) { return t.getAttribute("aria-selected") === "true"; })[0];
        moveInk(cur);
      });
    });
  }

  /* --- 卡片聚光（只在支持 hover 的指针设备上） -------------------------- */
  function initSpotlight() {
    if (reduceMotion) return;
    if (!window.matchMedia("(hover: hover) and (pointer: fine)").matches) return;
    document.querySelectorAll("[data-spot]").forEach(function (el) {
      el.addEventListener("pointermove", function (e) {
        var r = el.getBoundingClientRect();
        el.style.setProperty("--mx", ((e.clientX - r.left) / r.width) * 100 + "%");
        el.style.setProperty("--my", ((e.clientY - r.top) / r.height) * 100 + "%");
      });
    });
  }

  /* --- 网格流光：沿 88px 网格线随机划过 -------------------------------- */
  function initGridLights() {
    if (reduceMotion) return;
    var layer = document.createElement("div");
    layer.className = "grid-lights";
    layer.setAttribute("aria-hidden", "true");
    document.body.appendChild(layer);

    function build() {
      layer.innerHTML = "";
      var vw = window.innerWidth;
      var vh = window.innerHeight;
      var count = vw < 760 ? 5 : 9;
      for (var i = 0; i < count; i++) {
        var horizontal = i % 2 === 0;
        var len = 70 + Math.random() * 130;
        var el = document.createElement("span");
        el.className = "gl " + (horizontal ? "gl--x" : "gl--y");
        var dur = 7 + Math.random() * 9;
        if (horizontal) {
          var k = Math.floor(Math.random() * Math.floor(vh / 88));
          el.style.top = (k * 88) + "px";
          el.style.left = "-" + (len + 40) + "px";
          el.style.width = len + "px";
          el.style.setProperty("--travel", (vw + len + 80) + "px");
          el.style.animation = "glide-x " + dur.toFixed(1) + "s linear " + (-Math.random() * dur).toFixed(1) + "s infinite";
        } else {
          var j = Math.floor(Math.random() * Math.floor(vw / 88));
          el.style.left = (j * 88) + "px";
          el.style.top = "-" + (len + 40) + "px";
          el.style.height = len + "px";
          el.style.setProperty("--travel", (vh + len + 80) + "px");
          el.style.animation = "glide-y " + dur.toFixed(1) + "s linear " + (-Math.random() * dur).toFixed(1) + "s infinite";
        }
        layer.appendChild(el);
      }
    }

    var rt;
    window.addEventListener("resize", function () {
      clearTimeout(rt);
      rt = setTimeout(build, 240);
    });
    build();
  }

  /* --- 首页系统小地图：悬停方块，连它里面的文字一起亮起 ----------------- */
  function initMapHover() {
    var svg = document.querySelector(".map-wrap svg");
    if (!svg) return;
    var caps = Array.prototype.slice.call(svg.querySelectorAll("text.map-cap"));
    svg.querySelectorAll("rect.map-box").forEach(function (rect) {
      var b;
      try { b = rect.getBBox(); } catch (e) { return; }
      var inside = caps.filter(function (t) {
        var x = parseFloat(t.getAttribute("x"));
        var y = parseFloat(t.getAttribute("y"));
        return isFinite(x) && isFinite(y) &&
          x >= b.x - 4 && x <= b.x + b.width + 4 &&
          y >= b.y - 4 && y <= b.y + b.height + 4;
      });
      function on() {
        rect.classList.add("is-hi");
        inside.forEach(function (t) { t.classList.add("is-hi"); });
      }
      function off() {
        rect.classList.remove("is-hi");
        inside.forEach(function (t) { t.classList.remove("is-hi"); });
      }
      rect.addEventListener("mouseenter", on);
      rect.addEventListener("mouseleave", off);
    });
  }

  /* --- 首屏碎片视差（仅暗金主题有碎片） -------------------------------- */
  function initHeroParallax() {
    var hero = document.querySelector(".hero");
    if (!hero || reduceMotion) return;
    var ticking = false;
    function update() {
      ticking = false;
      var y = Math.min(window.scrollY, window.innerHeight);
      hero.style.setProperty("--hero-shift", (y * 0.16).toFixed(1) + "px");
    }
    window.addEventListener("scroll", function () {
      if (!ticking) { ticking = true; requestAnimationFrame(update); }
    }, { passive: true });
    update();
  }

  /* --- 平滑锚点（补偿固定导航） ---------------------------------------- */
  function initAnchors() {
    document.querySelectorAll('a[href^="#"]').forEach(function (a) {
      a.addEventListener("click", function (e) {
        var id = a.getAttribute("href").slice(1);
        if (!id) return;
        var el = document.getElementById(id);
        if (!el) return;
        e.preventDefault();
        var top = el.getBoundingClientRect().top + window.scrollY - 84;
        window.scrollTo({ top: top, behavior: reduceMotion ? "auto" : "smooth" });
        history.replaceState(null, "", "#" + id);
      });
    });
  }

  document.addEventListener("DOMContentLoaded", function () {
    initNav();
    initNavMore();
    initProgress();
    initReveal();
    initCounters();
    initTabs();
    initSpotlight();
    initMapHover();
    initHeroParallax();
    initGridLights();
    initAnchors();
    document.documentElement.classList.add("ready");
  });

  /* --- 内容切换动画：换文字时整块淡入上移 ------------------------------- */
  function swapIn(el) {
    if (!el || reduceMotion) return;
    el.classList.remove("swap-in");
    void el.offsetWidth;
    el.classList.add("swap-in");
  }

  window.RC = { reduceMotion: reduceMotion, easeOut: easeOut, swapIn: swapIn };
})();
