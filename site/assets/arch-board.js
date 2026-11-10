/* recollect — architecture board: 连线、流动点、检视器、哈希合成器 */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------------- 检视器数据 ---------------- */
  var DATA = {
    agents: {
      id: "host",
      title: "设备上的 agent",
      sub: "会话的产出方，但不是记忆的所有者",
      rows: [
        ["来源", "Claude Code · Qoder · DSH · MiniMax"],
        ["入口", "各自实现转换器，交给入库契约"],
        ["原则", "核心不认识任何具体 agent；各家专用的东西一律外置"]
      ],
      note: "「统一」必须无损：转换器校验不过就改转换器，核心不解析任何私有格式。"
    },
    converter: {
      id: "adapter",
      title: "转换器",
      sub: "把各家自己的会话，翻译成统一的入库格式",
      rows: [
        ["校验器", "ingest_validate"],
        ["契约", "v2 已上线；v3 字段已经有了，还没接进入库路径"],
        ["事件字段", "seq / parent_seq / kind / speaker_id / ts_ms / text / tool / thread / source_ref"],
        ["拒收", "不认识的字段、扫到秘密、工具调用和结果对不上"]
      ],
      note: "「统一」必须不丢东西：校验不过就改转换器，核心不解析任何私有格式。"
    },
    node: {
      id: "node",
      title: "recollect-node",
      sub: "边缘上的同步、只读副本与离线整理",
      rows: [
        ["私钥", "device.key"],
        ["副本", "replica/ 整份下载，只读不写"],
        ["暂存", "pending.db，断满一天后的本地整理批次"],
        ["灾备", "dr/ 每小时自备份，没确认的永远不删"],
        ["状态", "node-state.json"],
        ["命令", "node --code / --once / --push / --pull"]
      ],
      note: "数据源三种：live（直接读中心）、replica（只读副本）、pending_reconcile（本地整理过、还没并回中心）。写入一律走本机。"
    },
    consume: {
      id: "surface",
      title: "使用入口",
      sub: "记忆进入 agent 的三条通道",
      rows: [
        ["mcp", "长驻进程；按 agent 角色挂工具目录"],
        ["hook", "brief（常驻简报）· probe（按需探针）"],
        ["doctor", "不输出正文、路径和密钥，只报结构与指纹"],
        ["视图", "brief / card / json / expand"],
        ["原话", "search / session / turn（远端优先，断线回落）"]
      ],
      note: "recollect_recall 默认 view=brief：一句自然语言简报 + [n] 句柄；句子由程序拼，读时不调模型；程序要结构化就读 view=json。"
    },
    hub: {
      id: "primary",
      title: "hub · 中心",
      sub: "存储权威，自己不需要有 agent",
      rows: [
        ["原始对话", "transcript.db，每轮原文，只增不改"],
        ["记忆本体", "memory.db，25 张正式核心表 + 7 张侧表（第 6 版）"],
        ["私有小本", "core.db，每 agent 一份加密内容和随手记，由 agent 自己写"],
        ["账本", "ledger.db，配额与进度，不出中心"],
        ["端点", "pair / challenge / ingest / rows / state + snapshot/memory + transcript/*"],
        ["职责", "解析指针 · 记归属 · 排抽取 · 审核 · 审计"],
        ["互读", "同一主人名下的 agent 默认互相可见；主机、账号这类环境信息仍隔离"]
      ],
      note: "中心自己也有 agent 时，它只是可选的使用者或抽取工人，不因此获得存储权威。"
    },
    standby: {
      id: "replica",
      title: "standby · 备份",
      sub: "追平之后才能接管",
      rows: [
        ["复制", "按 commit_seq 一条条增量追；这套序号只活在复制层"],
        ["接班", "追平 + 核对通过 → 换一代凭证，旧中心被挡住"],
        ["缺条目", "unavailable + gap，不拿旧内容冒充"],
        ["复制面", "记忆 / 索引 / 抽取 run / 授权 / 审计 / 配置"],
        ["不复制", "设备密钥与密钥箱单独决定，不跟着自动复制"]
      ],
      note: "指针在接班前后不变；旧中心被挡在新一代之外，再写也进不来。"
    }
  };

  var board = document.querySelector("[data-board]");
  if (board) initBoard();

  function initBoard() {
    var svg = board.querySelector("[data-wires]");
    var nodes = {};
    board.querySelectorAll("[data-node]").forEach(function (el) {
      nodes[el.getAttribute("data-node")] = el;
    });

    var WIRES = [
      { from: "agents", to: "converter" },
      { from: "converter", to: "node" },
      { from: "node", to: "hub", hot: true },
      { from: "hub", to: "node", dashed: true, tone: "slate" },
      { from: "hub", to: "standby", tone: "sage" },
      { from: "standby", to: "hub", dashed: true },
      { from: "node", to: "consume" }
    ];

    var NS = "http://www.w3.org/2000/svg";
    var dots = [];
    var activeKey = "node";
    var raf = 0;
    var last = 0;
    var visible = true;

    function rectOf(el) {
      var b = el.getBoundingClientRect();
      var root = board.getBoundingClientRect();
      return {
        cx: b.left - root.left + b.width / 2,
        cy: b.top - root.top + b.height / 2,
        left: b.left - root.left,
        right: b.right - root.left,
        top: b.top - root.top,
        bottom: b.bottom - root.top
      };
    }

    function build() {
      while (svg.firstChild) svg.removeChild(svg.firstChild);
      dots = [];

      WIRES.forEach(function (w) {
        var a = rectOf(nodes[w.from]);
        var b = rectOf(nodes[w.to]);
        var dx = b.cx - a.cx;
        var dy = b.cy - a.cy;
        var horizontal = Math.abs(dx) > Math.abs(dy) * 1.1;
        var d;

        if (horizontal) {
          var x1 = dx > 0 ? a.right : a.left;
          var x2 = dx > 0 ? b.left : b.right;
          var mx = (x1 + x2) / 2;
          d = "M" + x1 + " " + a.cy +
              " C" + mx + " " + a.cy + " " + mx + " " + b.cy +
              " " + x2 + " " + b.cy;
        } else {
          var y1 = dy > 0 ? a.bottom : a.top;
          var y2 = dy > 0 ? b.top : b.bottom;
          d = "M" + a.cx + " " + y1 + " L" + a.cx + " " + y2;
        }

        var path = document.createElementNS(NS, "path");
        path.setAttribute("d", d);
        path.setAttribute("data-from", w.from);
        path.setAttribute("data-to", w.to);
        if (w.dashed) path.setAttribute("class", "dash");
        if (w.hot) path.classList.add("is-hot");
        svg.appendChild(path);

        var len = path.getTotalLength();

        if (w.label) {
          var mid = path.getPointAtLength(len * (w.at || 0.5));
          var t = document.createElementNS(NS, "text");
          t.setAttribute("x", mid.x);
          t.setAttribute("y", mid.y + (w.dy || 0));
          t.setAttribute("text-anchor", "middle");
          t.textContent = w.label;
          svg.appendChild(t);
        }

        if (!horizontal || reduce) {
          /* 同列短连接：静态中点 */
          if (!horizontal) {
            var p = path.getPointAtLength(len / 2);
            var c = document.createElementNS(NS, "circle");
            c.setAttribute("cx", p.x);
            c.setAttribute("cy", p.y);
            c.setAttribute("r", 1.8);
            c.setAttribute("fill", "rgba(200,164,104,.55)");
            svg.appendChild(c);
          }
          return;
        }

        var n = len > 260 ? 2 : 1;
        for (var i = 0; i < n; i++) {
          var dot = document.createElementNS(NS, "circle");
          dot.setAttribute("r", 3);
          dot.setAttribute("class", "flow" + (w.tone ? " " + w.tone : ""));
          svg.appendChild(dot);
          dots.push({
            el: dot,
            path: path,
            from: w.from,
            to: w.to,
            len: len,
            t: i / n + Math.random() * 0.25,
            speed: 46 + Math.random() * 26
          });
        }
      });

      svg.querySelectorAll("path").forEach(function (p) {
        var related = p.getAttribute("data-from") === activeKey || p.getAttribute("data-to") === activeKey;
        p.classList.toggle("is-dim", !related);
      });
    }

    function tick(ts) {
      if (!visible) { raf = requestAnimationFrame(tick); return; }
      var dt = Math.min((ts - last) / 1000, 0.05);
      last = ts;
      dots.forEach(function (d) {
        d.t = (d.t + (d.speed * dt) / d.len) % 1;
        var p = d.path.getPointAtLength(d.t * d.len);
        d.el.setAttribute("cx", p.x);
        d.el.setAttribute("cy", p.y);
        var base = d.t > 0.94 ? (1 - d.t) / 0.06 : d.t < 0.05 ? d.t / 0.05 : 1;
        var related = d.from === activeKey || d.to === activeKey;
        d.el.style.opacity = base * (related ? 1 : 0.12);
      });
      raf = requestAnimationFrame(tick);
    }

    /* 检视器 */
    var insp = document.querySelector("[data-insp]");
    var inspHead = insp.querySelector(".insp__head");
    var inspNote = insp.querySelector("[data-insp-note]");
    function select(key) {
      var d = DATA[key];
      if (!d) return;
      document.querySelectorAll("[data-node]").forEach(function (el) {
        el.classList.toggle("is-active", el.getAttribute("data-node") === key);
      });
      activeKey = key;
      svg.querySelectorAll("path").forEach(function (p) {
        var related = p.getAttribute("data-from") === key || p.getAttribute("data-to") === key;
        p.classList.toggle("is-dim", !related);
      });
      insp.querySelector("[data-insp-id]").textContent = d.id;
      insp.querySelector("[data-insp-title]").textContent = d.title;
      insp.querySelector("[data-insp-sub]").textContent = d.sub;
      inspNote.textContent = d.note;
      var body = insp.querySelector("[data-insp-body]");
      body.innerHTML = "";
      d.rows.forEach(function (r, i) {
        var row = document.createElement("div");
        row.className = "insp__row swap-row";
        row.style.setProperty("--i", i);
        var k = document.createElement("div");
        k.className = "insp__k";
        k.textContent = r[0];
        var v = document.createElement("div");
        v.className = "insp__v";
        v.textContent = r[1];
        row.appendChild(k);
        row.appendChild(v);
        body.appendChild(row);
      });
      if (window.RC) {
        window.RC.swapIn(inspHead);
        window.RC.swapIn(inspNote);
      }
    }

    Object.keys(nodes).forEach(function (k) {
      nodes[k].addEventListener("click", function () { select(k); });
    });

    if ("IntersectionObserver" in window) {
      new IntersectionObserver(function (es) { visible = es[0].isIntersecting; }, { threshold: 0 })
        .observe(board);
    }

    var rt;
    window.addEventListener("resize", function () {
      clearTimeout(rt);
      rt = setTimeout(build, 160);
    });

    build();
    select("node");

    if (!reduce) {
      last = performance.now();
      raf = requestAnimationFrame(tick);
    }
  }

  /* ---------------- 哈希合成器 ---------------- */
  var lab = document.querySelector("[data-hash-tokens]");
  if (lab) initHash();

  function initHash() {
    var exprEl = document.querySelector("[data-hash-expr]");
    var outEl = document.querySelector("[data-hash-out]");
    var footEl = document.querySelector("[data-hash-foot]");
    var toks = Array.prototype.slice.call(lab.querySelectorAll("[data-tok]"));
    var seq = 0;

    function fallbackHex(s) {
      var h1 = 0x811c9dc5, h2 = 0xc2b2ae35;
      for (var i = 0; i < s.length; i++) {
        var c = s.charCodeAt(i);
        h1 = (h1 ^ c) * 0x01000193 >>> 0;
        h2 = (h2 ^ (c + i)) * 0x85ebca6b >>> 0;
      }
      var out = "";
      var a = h1, b = h2;
      for (var k = 0; k < 8; k++) {
        a = (a * 1664525 + 1013904223) >>> 0;
        b = (b * 22695477 + 1) >>> 0;
        out += a.toString(16).padStart(8, "0") + b.toString(16).padStart(8, "0");
      }
      return out.slice(0, 64);
    }

    function digest(text) {
      if (window.crypto && window.crypto.subtle && window.isSecureContext) {
        return crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)).then(function (buf) {
          return Array.prototype.map.call(new Uint8Array(buf), function (b) {
            return b.toString(16).padStart(2, "0");
          }).join("");
        });
      }
      return Promise.resolve(fallbackHex(text));
    }

    function update() {
      var parts = toks.filter(function (t) { return t.getAttribute("aria-pressed") === "true"; })
        .map(function (t) { return t.getAttribute("data-tok"); });
      exprEl.textContent = parts.length ? parts.join(" | ") : "（空输入）";
      if (!parts.length) {
        outEl.textContent = "—";
        footEl.textContent = "没有字段，就没有身份";
        return;
      }
      var myseq = ++seq;
      digest(parts.join("|")).then(function (hex) {
        if (myseq !== seq) return;
        outEl.textContent = hex.slice(0, 32) + " …";
        outEl.classList.remove("flash");
        void outEl.offsetWidth;
        outEl.classList.add("flash");
        footEl.textContent = "sha-256 · 64 hex · 本机实时计算 · 改一个字符就全变";
      });
    }

    toks.forEach(function (t) {
      t.addEventListener("click", function () {
        var on = t.getAttribute("aria-pressed") === "true";
        t.setAttribute("aria-pressed", on ? "false" : "true");
        update();
      });
    });

    update();
  }
})();
