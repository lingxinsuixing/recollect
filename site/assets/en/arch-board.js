/* recollect — architecture board: wires, flow dots, inspector, hash composer */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------------- inspector data ---------------- */
  var DATA = {
    agents: {
      id: "host",
      title: "Agent on the device",
      sub: "It produces sessions, but does not own the memory",
      rows: [
        ["Source", "Claude Code · Qoder · DSH · MiniMax"],
        ["Entry", "each one implements its own converter, against the ingest contract"],
        ["Rule", "the core knows no specific agent; anything vendor-specific lives outside it"]
      ],
      note: "\"Uniform\" must be lossless: if the converter fails validation, change the converter — the core parses no private format."
    },
    converter: {
      id: "adapter",
      title: "Converter",
      sub: "Translates each vendor's own session format into one ingest format",
      rows: [
        ["Validator", "ingest_validate"],
        ["Contract", "v2 is live; v3 fields already exist but are not yet wired into the ingest path"],
        ["Event fields", "seq / parent_seq / kind / speaker_id / ts_ms / text / tool / thread / source_ref"],
        ["Rejects", "unknown fields, a detected secret, a tool call that does not match its result"]
      ],
      note: "\"Uniform\" must lose nothing: if validation fails, change the converter — the core parses no private format."
    },
    node: {
      id: "node",
      title: "recollect-node",
      sub: "Sync, read-only replica and offline distillation on the edge",
      rows: [
        ["Private key", "device.key"],
        ["Replica", "replica/ downloaded in full, read-only, never written"],
        ["Staging", "pending.db, local distillation batches after a full day offline"],
        ["DR", "dr/ self-backup every hour; unconfirmed data is never deleted"],
        ["State", "node-state.json"],
        ["Commands", "node --code --once --push --pull"]
      ],
      note: "Three data sources: live (read the hub directly), replica (read-only copy), pending_reconcile (distilled locally, not yet merged back into the hub). Writes always go through this machine."
    },
    consume: {
      id: "surface",
      title: "Entry points",
      sub: "Three channels through which memory reaches an agent",
      rows: [
        ["mcp", "a long-running process; mounts tool directories by agent role"],
        ["hook", "brief (always-on brief) · probe (on-demand probe)"],
        ["doctor", "prints no text, paths or keys — only structure and fingerprints"],
        ["Views", "brief / card / json / expand"],
        ["Raw words", "search / session / turn (remote first, local fallback)"]
      ],
      note: "recollect_recall defaults to view=brief: one natural-language brief plus [n] handles; the sentences are assembled by code, with no model call at read time; ask for view=json when the program wants structure."
    },
    hub: {
      id: "primary",
      title: "hub · primary",
      sub: "Holds the storage authority; needs no agent of its own",
      rows: [
        ["Raw transcript", "transcript.db, raw text per turn, append-only"],
        ["Memory body", "memory.db, 25 formal core tables + 7 side tables (schema v6)"],
        ["Private notebook", "core.db, one encrypted body plus notes per agent, written by the agent itself"],
        ["Ledger", "ledger.db, quota and progress, never leaves the hub"],
        ["Endpoints", "pair / challenge / ingest / rows / state + snapshot/memory + transcript/*"],
        ["Duties", "resolve pointers · record attribution · schedule extraction · review · audit"],
        ["Mutual read", "agents under the same owner see each other by default; environment details such as host and account stay isolated"]
      ],
      note: "When the hub also runs agents, they are optional consumers or extraction workers, and gain no storage authority from it."
    },
    standby: {
      id: "replica",
      title: "standby · backup",
      sub: "It can take over only after catching up",
      rows: [
        ["Replication", "catches up one increment at a time by commit_seq; those numbers live only in the replication layer"],
        ["Takeover", "caught up + check passed → rotate the credential generation, the old hub gets blocked"],
        ["Missing rows", "unavailable + gap, never passing off old content as current"],
        ["Replicated surfaces", "memory / index / extraction runs / grants / audit / config"],
        ["Not replicated", "device keys and the secret box are decided separately, never replicated automatically"]
      ],
      note: "Pointers do not change across takeover; the old hub sits outside the new generation and its writes no longer land."
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
          /* short same-column link: static midpoint */
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

    /* inspector */
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

  /* ---------------- hash composer ---------------- */
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
      exprEl.textContent = parts.length ? parts.join(" | ") : "(empty input)";
      if (!parts.length) {
        outEl.textContent = "—";
        footEl.textContent = "No fields, no identity";
        return;
      }
      var myseq = ++seq;
      digest(parts.join("|")).then(function (hex) {
        if (myseq !== seq) return;
        outEl.textContent = hex.slice(0, 32) + " …";
        outEl.classList.remove("flash");
        void outEl.offsetWidth;
        outEl.classList.add("flash");
        footEl.textContent = "sha-256 · 64 hex · computed live on this machine · one changed character changes all";
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
