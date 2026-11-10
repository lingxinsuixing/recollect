/* recollect — memory page: 三层展开、实体图 hover、句柄展开 */
(function () {
  "use strict";

  /* ---------- 三层 ---------- */
  var layers = Array.prototype.slice.call(document.querySelectorAll("[data-layer]"));
  function layerBody(l) { return l.querySelector(".layer__body"); }
  function sizeLayer(l, open) {
    var b = layerBody(l);
    if (!b) return;
    b.style.maxHeight = open ? b.scrollHeight + "px" : "";
  }
  layers.forEach(function (l) {
    l.addEventListener("click", function () {
      var open = l.classList.contains("is-open");
      layers.forEach(function (x) {
        x.classList.remove("is-open");
        x.setAttribute("aria-expanded", "false");
        sizeLayer(x, false);
      });
      if (!open) {
        l.classList.add("is-open");
        l.setAttribute("aria-expanded", "true");
        sizeLayer(l, true);
      }
    });
  });
  window.addEventListener("resize", function () {
    layers.forEach(function (l) {
      if (l.classList.contains("is-open")) sizeLayer(l, true);
    });
  }, { passive: true });

  /* ---------- 召回句柄 ---------- */
  document.querySelectorAll("[data-raw]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var id = btn.getAttribute("data-raw");
      var target = document.getElementById("raw-" + id.slice(1));
      if (!target) return;
      var open = !target.classList.contains("is-open");
      target.classList.toggle("is-open", open);
      target.style.maxHeight = open ? target.scrollHeight + "px" : "0px";
      btn.classList.toggle("is-open", open);
      btn.setAttribute("aria-expanded", open ? "true" : "false");
    });
  });
  window.addEventListener("resize", function () {
    document.querySelectorAll(".rawline.is-open").forEach(function (el) {
      el.style.maxHeight = el.scrollHeight + "px";
    });
  }, { passive: true });

  /* ---------- 实体图 ---------- */
  var graph = document.querySelector(".graphbox");
  if (!graph) return;

  var ENT = {
    anchor: {
      k: "anchor · 锚",
      t: "阿岚（示例主体）",
      d: "一个实体锚。所有关于她的称呼、属性、状态与事件都挂在这里；换一处措辞不会变成另一个人。",
      rows: [["锚类型", "person"], ["别名", "若若 · 岚姐"], ["提及", "3 处 · 2 个说话人"]]
    },
    alias: {
      k: "alias · 称呼",
      t: "称呼「若若」",
      d: "称呼只是别名，不是另一个人。「若若」和「岚姐」都归到同一个锚上，每次出现的位置也都留着。",
      rows: [["出现", "2026-10-12 · 你"], ["归并", "anchor#a41"], ["状态", "已确认"]]
    },
    mention1: {
      k: "mention · 提及",
      t: "提及「她」",
      d: "「她」这种代词不指向具体的人。程序先把可能的候选列出来，模型只做一次判断；判不了就留空，不硬猜。",
      rows: [["出现", "2026-10-19 · 你"], ["候选", "阿岚 / 阿K"], ["归属", "阿岚 · 依据上下文"]]
    },
    mention2: {
      k: "mention · 提及",
      t: "提及「岚姐」",
      d: "同一个人，另一种叫法。记忆里存的是「归到哪个锚」，不是这几个字本身。",
      rows: [["出现", "2026-10-25 · 阿K"], ["归并", "anchor#a41"], ["span", "88–92"]]
    },
    attribute: {
      k: "attribute · 属性",
      t: "属性：偏好 深夜写代码",
      d: "属性会变：喜欢什么、做什么、在意什么。新的一条追加在前面，旧的不覆盖，随时能翻回去看是什么时候变的。",
      rows: [["类别", "preference"], ["记于", "2026-10-20"], ["出处", "turn t-7f1c"]]
    },
    state: {
      k: "state_assertion · 状态",
      t: "状态：疲惫 · 10-25",
      d: "情绪单独存，不写进属性。它是某一刻的状态，带时间，也带有效期；过期就自然淡出。",
      rows: [["状态", "疲惫"], ["记于", "2026-10-25"], ["主体", "阿岚"]]
    },
    relation: {
      k: "relation · 关系",
      t: "关系：同事 · 阿K",
      d: "关系连着两个人：什么关系、从什么时候开始。关系变了就加一条新的，旧的留着。",
      rows: [["类型", "colleague"], ["方向", "无向"], ["自", "2026-10-12"]]
    },
    episode: {
      k: "episode · 事件",
      t: "事件：10-24 部署讨论",
      d: "事件记「什么时候、发生了什么、后来怎样」，连着一组相关事实；它回答发生了什么，不回答她是个什么样的人。",
      rows: [["时间", "2026-10-24"], ["参与者", "你 · 阿岚"], ["相关事实", "2 条"]]
    }
  };

  var entMap = {
    mention1: "mention",
    mention2: "mention2"
  };

  var nodes = Array.prototype.slice.call(graph.querySelectorAll("[data-ent]"));
  var edges = Array.prototype.slice.call(graph.querySelectorAll(".g-edge"));
  var labels = Array.prototype.slice.call(graph.querySelectorAll(".g-label"));
  var panel = document.querySelector("[data-entpanel]");
  var pick = document.querySelector("[data-entpick]");

  if (pick) {
    nodes.forEach(function (n) {
      var key = n.getAttribute("data-ent");
      var b = document.createElement("button");
      b.type = "button";
      b.setAttribute("data-pick", key);
      b.textContent = n.getAttribute("aria-label") || key;
      b.addEventListener("click", function () { highlight(key); });
      pick.appendChild(b);
    });
  }

  function syncPick(key) {
    if (!pick) return;
    pick.querySelectorAll("[data-pick]").forEach(function (b) {
      b.classList.toggle("is-on", b.getAttribute("data-pick") === key);
    });
  }

  var lastKey = null;
  function render(key) {
    var d = ENT[key];
    if (!d || !panel) return;
    var changed = key !== lastKey;
    lastKey = key;
    panel.querySelector("[data-ent-k]").textContent = d.k;
    panel.querySelector("[data-ent-t]").textContent = d.t;
    panel.querySelector("[data-ent-d]").textContent = d.d;
    var rows = panel.querySelector("[data-ent-rows]");
    rows.innerHTML = "";
    d.rows.forEach(function (r, i) {
      var row = document.createElement("div");
      row.className = "entpanel__row swap-row";
      row.style.setProperty("--i", i);
      var a = document.createElement("span");
      a.textContent = r[0];
      var b = document.createElement("span");
      b.textContent = r[1];
      row.appendChild(a);
      row.appendChild(b);
      rows.appendChild(row);
    });
    if (changed && window.RC) window.RC.swapIn(panel);
  }

  function highlight(key) {
    var edgeKey = entMap[key] || key;
    nodes.forEach(function (n) {
      var on = n.getAttribute("data-ent") === key || n.getAttribute("data-ent") === "anchor";
      n.classList.toggle("is-hot", on);
      n.classList.toggle("is-dim", !on);
    });
    edges.forEach(function (e) {
      var on = key === "anchor" || e.getAttribute("data-edge") === edgeKey;
      e.classList.toggle("is-hot", on);
      e.classList.toggle("is-dim", !on);
    });
    labels.forEach(function (l) { l.classList.add("is-dim"); });
    syncPick(key);
    render(key);
  }

  function clear() {
    nodes.forEach(function (n) { n.classList.remove("is-hot", "is-dim"); });
    edges.forEach(function (e) { e.classList.remove("is-hot", "is-dim"); });
    labels.forEach(function (l) { l.classList.remove("is-dim"); });
    syncPick("anchor");
    render("anchor");
  }

  nodes.forEach(function (n) {
    var key = n.getAttribute("data-ent");
    n.addEventListener("mouseenter", function () { highlight(key); });
    n.addEventListener("focus", function () { highlight(key); });
    n.addEventListener("mouseleave", clear);
    n.addEventListener("blur", clear);
    n.addEventListener("click", function () { highlight(key); });
  });

  syncPick("anchor");
  render("anchor");
})();
