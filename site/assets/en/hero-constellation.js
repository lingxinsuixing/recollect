/* recollect — hero constellation
   记忆图谱：锚点 / mention / 事实-事件 / 指针边 + 沿边传播的召回脉冲。
   独立运行的 canvas，尊重 prefers-reduced-motion。 */
(function () {
  "use strict";

  var canvas = document.querySelector("[data-constellation]");
  if (!canvas) return;

  var ctx = canvas.getContext("2d");
  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var GOLD = document.documentElement.getAttribute("data-theme") === "gold";
  var SMALL = window.innerWidth < 760;
  var dpr = Math.min(window.devicePixelRatio || 1, SMALL ? 1.5 : 2);

  var W = 0, H = 0;
  var nodes = [], edges = [], pulses = [], dust = [];
  var rings = { angle: 0 };
  var pointer = { x: 0, y: 0, tx: 0, ty: 0, active: false };
  var visible = true;
  var raf = 0;
  var last = 0;
  var start = performance.now();

  /* 确定性随机，保证每次打开构图一致 */
  function mulberry32(a) {
    return function () {
      a |= 0; a = (a + 0x6d2b79f5) | 0;
      var t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  var MONO_FONT = '9px "Cascadia Mono", Consolas, monospace';

  var LABELS = [
    "anchor", "mention", "fact", "episode", "state",
    "card", "pointer", "session_uid", "turn_uid", "as_of"
  ];

  function resize() {
    var rect = canvas.getBoundingClientRect();
    W = rect.width;
    H = rect.height;
    canvas.width = Math.floor(W * dpr);
    canvas.height = Math.floor(H * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    build();
  }

  function build() {
    var rnd = mulberry32(20261029);
    nodes = [];
    edges = [];
    pulses = [];
    dust = [];

    var count = Math.round(Math.min(92, Math.max(40, (W * H) / 22000)));
    if (SMALL) count = Math.round(count * 0.6);
    var clusters = [
      { x: W * 0.70, y: H * 0.40, r: Math.min(W, H) * 0.20 },
      { x: W * 0.86, y: H * 0.66, r: Math.min(W, H) * 0.15 },
      { x: W * 0.57, y: H * 0.70, r: Math.min(W, H) * 0.13 }
    ];

    for (var i = 0; i < count; i++) {
      var c = clusters[i % clusters.length];
      var ang = rnd() * Math.PI * 2;
      var rad = Math.sqrt(rnd()) * c.r * 1.5;
      var x = c.x + Math.cos(ang) * rad + (rnd() - 0.5) * 40;
      var y = c.y + Math.sin(ang) * rad * 0.82 + (rnd() - 0.5) * 40;

      var kindRoll = rnd();
      var kind = kindRoll > 0.90 ? "anchor" : kindRoll > 0.74 ? "structure" : "leaf";

      nodes.push({
        x: x, y: y,
        ox: x, oy: y,
        vx: (rnd() - 0.5) * 0.16,
        vy: (rnd() - 0.5) * 0.16,
        r: kind === "anchor" ? 2.4 + rnd() * 1.4 : kind === "structure" ? 1.5 + rnd() * 1.0 : 0.9 + rnd() * 0.9,
        kind: kind,
        phase: rnd() * Math.PI * 2,
        label: (i % 7 === 0 && rnd() > 0.25) ? LABELS[i % LABELS.length] : null
      });
    }

    for (var dd = 0; dd < (SMALL ? 22 : 42); dd++) {
      dust.push({
        x: rnd() * W,
        y: rnd() * H,
        r: 0.4 + rnd() * 0.9,
        v: 0.03 + rnd() * 0.13,
        a: 0.04 + rnd() * 0.10,
        ph: rnd() * Math.PI * 2
      });
    }

    /* 边：按距离连，锚点优先多连 */
    var maxD = Math.min(W, H) * 0.30;
    for (var a = 0; a < nodes.length; a++) {
      for (var b = a + 1; b < nodes.length; b++) {
        var dx = nodes[a].x - nodes[b].x;
        var dy = nodes[a].y - nodes[b].y;
        var d = Math.hypot(dx, dy);
        var lim = (nodes[a].kind === "anchor" || nodes[b].kind === "anchor") ? maxD * 1.15 : maxD;
        if (d < lim && rnd() > d / lim) {
          edges.push({ a: a, b: b, d: d, born: rnd() * -6, life: 4 + rnd() * 7 });
        }
      }
    }
  }

  function spawnPulse(now) {
    if (!edges.length) return;
    if (pulses.length > 6) return;
    var e = edges[Math.floor(Math.random() * edges.length)];
    pulses.push({
      edge: e,
      t: 0,
      speed: 0.00055 + Math.random() * 0.0004,
      hue: Math.random() > 0.62 ? "slate" : "brass",
      tail: []
    });
  }

  function diamond(x, y, r, fill) {
    ctx.beginPath();
    ctx.moveTo(x, y - r);
    ctx.lineTo(x + r, y);
    ctx.lineTo(x, y + r);
    ctx.lineTo(x - r, y);
    ctx.closePath();
    ctx.fillStyle = fill;
    ctx.fill();
  }

  function drawRings(t) {
    var cx = W * 0.74;
    var cy = H * 0.5;
    var base = Math.min(W, H);
    rings.angle += 0.00012;

    ctx.save();
    ctx.translate(cx, cy);
    ctx.rotate(rings.angle);

    [0.46, 0.34, 0.22].forEach(function (k, idx) {
      var r = base * k;
      ctx.beginPath();
      ctx.arc(0, 0, r, 0, Math.PI * 2);
      ctx.strokeStyle = GOLD
        ? (idx === 1 ? "rgba(200,155,79,0.16)" : "rgba(232,212,170,0.07)")
        : (idx === 1 ? "rgba(200,164,104,0.11)" : "rgba(241,237,229,0.055)");
      ctx.lineWidth = 1;
      ctx.stroke();

      if (idx !== 2) {
        var ticks = idx === 0 ? 60 : 36;
        for (var i = 0; i < ticks; i++) {
          var ang = (i / ticks) * Math.PI * 2;
          var len = i % 5 === 0 ? 7 : 3;
          ctx.beginPath();
          ctx.moveTo(Math.cos(ang) * (r - len), Math.sin(ang) * (r - len));
          ctx.lineTo(Math.cos(ang) * r, Math.sin(ang) * r);
          ctx.strokeStyle = "rgba(241,237,229,0.10)";
          ctx.stroke();
        }
      }
    });

    /* 十字准线 */
    ctx.strokeStyle = "rgba(241,237,229,0.05)";
    ctx.beginPath();
    ctx.moveTo(-base * 0.5, 0); ctx.lineTo(base * 0.5, 0);
    ctx.moveTo(0, -base * 0.5); ctx.lineTo(0, base * 0.5);
    ctx.stroke();

    ctx.restore();
  }

  function draw(now) {
    var t = now - start;
    var dt = Math.min(now - last, 40);
    last = now;

    /* 视差 */
    pointer.x += (pointer.tx - pointer.x) * 0.05;
    pointer.y += (pointer.ty - pointer.y) * 0.05;

    ctx.clearRect(0, 0, W, H);

    drawRings(t);

    /* 指针微光：只在有指针时出现，很淡 */
    if (pointer.active) {
      var pg = ctx.createRadialGradient(pointer.x, pointer.y, 0, pointer.x, pointer.y, 230);
      pg.addColorStop(0, GOLD ? "rgba(200,155,79,0.055)" : "rgba(200,164,104,0.045)");
      pg.addColorStop(1, "rgba(0,0,0,0)");
      ctx.fillStyle = pg;
      ctx.fillRect(pointer.x - 230, pointer.y - 230, 460, 460);
    }

    /* 浮尘：很慢，向上漂 */
    for (var u = 0; u < dust.length; u++) {
      var d = dust[u];
      d.y -= d.v * (dt / 16);
      if (d.y < -12) { d.y = H + 12; d.x = Math.random() * W; }
      var dx = Math.sin(t * 0.00018 + d.ph) * 10;
      ctx.beginPath();
      ctx.arc(d.x + dx, d.y, d.r, 0, Math.PI * 2);
      ctx.fillStyle = GOLD
        ? "rgba(232,212,170," + d.a + ")"
        : "rgba(241,237,229," + d.a + ")";
      ctx.fill();
    }

    /* 位置推进 */
    for (var i = 0; i < nodes.length; i++) {
      var n = nodes[i];
      n.x += n.vx * (dt / 16);
      n.y += n.vy * (dt / 16);

      /* 回到原点附近（软约束） */
      n.vx += (n.ox - n.x) * 0.00025;
      n.vy += (n.oy - n.y) * 0.00025;

      /* 指针斥力 */
      if (pointer.active) {
        var px = pointer.x - n.x;
        var py = pointer.y - n.y;
        var pd = Math.hypot(px, py);
        if (pd < 150 && pd > 0.001) {
          var f = (150 - pd) / 150;
          n.vx -= (px / pd) * f * 0.055;
          n.vy -= (py / pd) * f * 0.055;
        }
      }

      n.vx *= 0.985;
      n.vy *= 0.985;
    }

    /* 边 */
    for (var e = 0; e < edges.length; e++) {
      var ed = edges[e];
      var a = nodes[ed.a], b = nodes[ed.b];
      var dist = Math.hypot(a.x - b.x, a.y - b.y);
      if (dist > 300) continue;
      var alpha = Math.max(0, 0.14 * (1 - dist / 300));
      var highlighted = pointer.active && (Math.hypot(a.x - pointer.x, a.y - pointer.y) < 90 || Math.hypot(b.x - pointer.x, b.y - pointer.y) < 90);
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.lineTo(b.x, b.y);
      if (GOLD) {
        ctx.strokeStyle = highlighted ? "rgba(236,203,139," + Math.min(0.75, alpha * 3.4) + ")" : "rgba(216,196,158," + (alpha * 1.25) + ")";
        ctx.lineWidth = highlighted ? 1.6 : 1.1;
      } else {
        ctx.strokeStyle = highlighted ? "rgba(200,164,104," + (alpha * 2.6) + ")" : "rgba(241,237,229," + alpha + ")";
        ctx.lineWidth = highlighted ? 1 : 0.7;
      }
      ctx.stroke();
    }

    /* 节点 */
    for (var k = 0; k < nodes.length; k++) {
      var nd = nodes[k];
      var pulse = 0.5 + 0.5 * Math.sin(t * 0.0012 + nd.phase);
      var near = pointer.active && Math.hypot(nd.x - pointer.x, nd.y - pointer.y) < 150;

      if (GOLD) {
        if (nd.kind === "anchor") {
          ctx.beginPath();
          ctx.arc(nd.x, nd.y, nd.r + 8 + pulse * 2, 0, Math.PI * 2);
          ctx.fillStyle = "rgba(200,155,79,0.07)";
          ctx.fill();
          diamond(nd.x, nd.y, nd.r + 1.2, near ? "rgba(240,222,180,1)" : "rgba(200,155,79,0.95)");
        } else if (nd.kind === "structure") {
          diamond(nd.x, nd.y, nd.r + 0.6, near ? "rgba(216,214,198,0.95)" : "rgba(154,163,156,0.8)");
        } else {
          diamond(nd.x, nd.y, nd.r + 0.4, near ? "rgba(242,233,214,0.95)" : "rgba(232,212,170," + (0.3 + pulse * 0.22) + ")");
        }
      } else if (nd.kind === "anchor") {
        ctx.beginPath();
        ctx.arc(nd.x, nd.y, nd.r + 7 + pulse * 2, 0, Math.PI * 2);
        ctx.fillStyle = "rgba(200,164,104,0.06)";
        ctx.fill();
        ctx.beginPath();
        ctx.arc(nd.x, nd.y, nd.r, 0, Math.PI * 2);
        ctx.fillStyle = near ? "rgba(233,212,166,1)" : "rgba(200,164,104,0.92)";
        ctx.fill();
      } else if (nd.kind === "structure") {
        ctx.beginPath();
        ctx.arc(nd.x, nd.y, nd.r, 0, Math.PI * 2);
        ctx.fillStyle = near ? "rgba(170,192,222,0.95)" : "rgba(138,161,191,0.72)";
        ctx.fill();
      } else {
        ctx.beginPath();
        ctx.arc(nd.x, nd.y, nd.r, 0, Math.PI * 2);
        ctx.fillStyle = near ? "rgba(241,237,229,0.85)" : "rgba(241,237,229," + (0.26 + pulse * 0.22) + ")";
        ctx.fill();
      }

      if (nd.label) {
        ctx.font = MONO_FONT;
        ctx.fillStyle = near ? "rgba(200,164,104,0.95)" : "rgba(241,237,229,0.24)";
        ctx.fillText(nd.label, nd.x + 9, nd.y + 3);
      }
    }

    /* 召回脉冲 */
    if (Math.random() < (SMALL ? 0.002 : 0.005)) spawnPulse(now);

    for (var p = pulses.length - 1; p >= 0; p--) {
      var pu = pulses[p];
      pu.t += pu.speed * dt;
      if (pu.t > 1) { pulses.splice(p, 1); continue; }
      var ea = nodes[pu.edge.a], eb = nodes[pu.edge.b];
      var x = ea.x + (eb.x - ea.x) * pu.t;
      var y = ea.y + (eb.y - ea.y) * pu.t;

      pu.tail.push({ x: x, y: y });
      if (pu.tail.length > 9) pu.tail.shift();

      for (var q = 0; q < pu.tail.length - 1; q++) {
        var alpha2 = (q / pu.tail.length) * 0.5;
        ctx.beginPath();
        ctx.moveTo(pu.tail[q].x, pu.tail[q].y);
        ctx.lineTo(pu.tail[q + 1].x, pu.tail[q + 1].y);
        ctx.strokeStyle = pu.hue === "slate"
          ? "rgba(138,161,191," + alpha2 + ")"
          : "rgba(233,212,166," + alpha2 + ")";
        ctx.lineWidth = 1.4;
        ctx.stroke();
      }

      ctx.beginPath();
      ctx.arc(x, y, 2.2, 0, Math.PI * 2);
      ctx.fillStyle = pu.hue === "slate" ? "rgba(170,192,222,0.95)" : "rgba(240,222,180,0.95)";
      ctx.fill();

      ctx.beginPath();
      ctx.arc(x, y, 7, 0, Math.PI * 2);
      ctx.fillStyle = pu.hue === "slate" ? "rgba(138,161,191,0.10)" : "rgba(233,212,166,0.12)";
      ctx.fill();
    }

    raf = requestAnimationFrame(draw);
  }

  function renderStatic() {
    last = performance.now();
    pointer.active = false;
    ctx.clearRect(0, 0, W, H);
    drawRings(0);
    for (var e = 0; e < edges.length; e++) {
      var ed = edges[e], a = nodes[ed.a], b = nodes[ed.b];
      var d = Math.hypot(a.x - b.x, a.y - b.y);
      if (d > 300) continue;
      ctx.beginPath();
      ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y);
      ctx.strokeStyle = "rgba(241,237,229," + Math.max(0, 0.14 * (1 - d / 300)) + ")";
      ctx.lineWidth = 0.7;
      ctx.stroke();
    }
    for (var k = 0; k < nodes.length; k++) {
      var n = nodes[k];
      ctx.beginPath();
      ctx.arc(n.x, n.y, n.r, 0, Math.PI * 2);
      if (GOLD) {
        diamond(n.x, n.y, n.r + 0.6, n.kind === "anchor" ? "rgba(200,155,79,0.95)"
          : n.kind === "structure" ? "rgba(154,163,156,0.8)"
          : "rgba(232,212,170,0.36)");
      } else {
        ctx.fillStyle = n.kind === "anchor" ? "rgba(200,164,104,0.92)"
          : n.kind === "structure" ? "rgba(138,161,191,0.72)"
          : "rgba(241,237,229,0.30)";
        ctx.fill();
      }
      if (n.label) {
        ctx.font = MONO_FONT;
        ctx.fillStyle = "rgba(241,237,229,0.22)";
        ctx.fillText(n.label, n.x + 9, n.y + 3);
      }
    }
    canvas.classList.add("is-ready");
  }

  function loop(now) {
    if (!visible) { raf = requestAnimationFrame(loop); return; }
    draw(now);
  }

  function startLoop() {
    if (reduce) { renderStatic(); return; }
    last = performance.now();
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(loop);
    canvas.classList.add("is-ready");
  }

  window.addEventListener("pointermove", function (e) {
    if (reduce) return;
    pointer.active = true;
    pointer.tx = e.clientX - canvas.getBoundingClientRect().left;
    pointer.ty = e.clientY - canvas.getBoundingClientRect().top;
    pointer.x = pointer.tx;
    pointer.y = pointer.ty;
  }, { passive: true });

  window.addEventListener("pointerleave", function () { pointer.active = false; });

  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (entries) {
      visible = entries[0].isIntersecting;
    }, { threshold: 0 }).observe(canvas);
  }

  var rt;
  window.addEventListener("resize", function () {
    clearTimeout(rt);
    rt = setTimeout(function () {
      resize();
      if (reduce) renderStatic();
    }, 180);
  });

  resize();
  startLoop();
})();
