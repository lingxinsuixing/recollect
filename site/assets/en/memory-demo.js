/* recollect — memory page: three-layer expand, entity graph hover, handle expand */
(function () {
  "use strict";

  /* ---------- three layers ---------- */
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

  /* ---------- recall handles ---------- */
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

  /* ---------- entity graph ---------- */
  var graph = document.querySelector(".graphbox");
  if (!graph) return;

  var ENT = {
    anchor: {
      k: "anchor · entity anchor",
      t: "A-Lan (sample subject)",
      d: "One entity anchor. Every form of address, attribute, state, and event about her hangs here; rewording one of them does not make a different person.",
      rows: [["Anchor type", "person"], ["Aliases", "Ruoruo · Lanjie"], ["Mentions", "3 occurrences · 2 speakers"]]
    },
    alias: {
      k: "alias · form of address",
      t: "Called \"Ruoruo\"",
      d: "A form of address is only an alias, not another person. \"Ruoruo\" and \"Lanjie\" both resolve to the same anchor, and every place they appear is kept.",
      rows: [["Occurred", "2026-10-12 · you"], ["Merged into", "anchor#a41"], ["Status", "confirmed"]]
    },
    mention1: {
      k: "mention",
      t: "Mention: \"she\"",
      d: "A pronoun like \"she\" does not point at a specific person. Code lists the likely candidates first and the model makes one judgment; when it cannot decide, the field stays empty instead of guessing.",
      rows: [["Occurred", "2026-10-19 · you"], ["Candidates", "A-Lan / A-K"], ["Attribution", "A-Lan · from context"]]
    },
    mention2: {
      k: "mention",
      t: "Mention: \"Lanjie\"",
      d: "The same person, another name. What memory stores is which anchor it resolves to, not these particular characters.",
      rows: [["Occurred", "2026-10-25 · A-K"], ["Merged into", "anchor#a41"], ["span", "88–92"]]
    },
    attribute: {
      k: "attribute",
      t: "Attribute: preference, writes code late at night",
      d: "Attributes change: what someone likes, does, cares about. The new row is appended on top, the old one is not overwritten, and you can always go back to see when it changed.",
      rows: [["Kind", "preference"], ["Recorded", "2026-10-20"], ["Provenance", "turn t-7f1c"]]
    },
    state: {
      k: "state_assertion · state",
      t: "State: tired · 10-25",
      d: "Mood is stored separately, not inside attributes. It is the state at one moment, with a time and an expiry; when it goes stale it fades out.",
      rows: [["State", "tired"], ["Recorded", "2026-10-25"], ["Subject", "A-Lan"]]
    },
    relation: {
      k: "relation",
      t: "Relation: colleague · A-K",
      d: "A relation links two people: what the relation is, and since when. A change adds a new row and keeps the old one.",
      rows: [["Type", "colleague"], ["Direction", "undirected"], ["Since", "2026-10-12"]]
    },
    episode: {
      k: "episode · event",
      t: "Event: 10-24 deploy discussion",
      d: "An event records when it happened, what happened, and how it turned out, linked to a set of related facts; it answers what happened, not what kind of person she is.",
      rows: [["Time", "2026-10-24"], ["Participants", "you · A-Lan"], ["Related facts", "2"]]
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
