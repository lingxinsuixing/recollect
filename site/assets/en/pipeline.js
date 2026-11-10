/* recollect — pipeline: stage playback, duty switch, review gate */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------------- pipeline ---------------- */
  var STAGES = [
    { type: "P", label: "program", title: "ingest", desc: "Translates the agent's own session format into one unified format; unknown fields and scanned secrets are rejected outright.", input: "session.json (agent-private format) → converter", output: "events[] · each with seq / speaker_id / ts_ms / text" },
    { type: "P", label: "program", title: "sanitize", desc: "Sanitize: swap sensitive values for irreversible stand-ins; the raw text is neither deleted nor changed.", input: "events[]", output: "events[] + pii hit report" },
    { type: "P", label: "program", title: "locate", desc: "Ordinals, timestamps, speakers, tool calls and character offsets are all fixed by code; the model never touches them.", input: "events[]", output: "locators[] · turn_uid / span / speaker_id / ts_ms" },
    { type: "P", label: "program", title: "candidates", desc: "List who was mentioned first, then drop what is clearly not a person: tool names, UUIDs, paths, bare method names.", input: "locators[] + text", output: "candidates[] · each occurrence with a span" },
    { type: "L", label: "llm", title: "naming", desc: "Name entities and merge aliases; two different people who share a name are split here as well, without relying on string similarity.", input: "candidates[] + context", output: "anchors[] + alias[] (seq and quote only)" },
    { type: "P", label: "program", title: "expand mentions", desc: "Attach every mention to its anchor and keep each occurrence as its own row; nothing is merged into one.", input: "anchors[] + candidates[]", output: "mention[] · anchor × span" },
    { type: "L", label: "llm", title: "disambiguate", desc: "Pronouns and ambiguity get one judgement only: who \"she\" is in this instance. If it cannot be decided, leave it empty — no guessing, no default value.", input: "flagged mentions[]", output: "verdict[] or empty" },
    { type: "L", label: "llm", title: "facts", desc: "Fact extraction, one class at a time. Each result carries only seq and a verbatim quote; nothing to report returns empty.", input: "turn text + anchor", output: "{ seq, quote, facts[] }" },
    { type: "L", label: "llm", title: "events / relations / states", desc: "Events, relations and states are extracted separately. Structural relations such as reply chains and call/result pairs are linked by code first; the model only adds semantic relations.", input: "facts[] + structural edges", output: "episode[] / relation[] / state_assertion[]" },
    { type: "P", label: "program", title: "validate", desc: "Code check: subject, time and value must all be found in the quote; anything missing is sent back with a reason.", input: "all drafts", output: "passed[] + issues[]" },
    { type: "L", label: "llm", title: "review", desc: "Model review runs on its own: it only flags problems, never rewrites or re-extracts, and files each issue under converter / code / prompt / model.", input: "passed[]", output: "review_decision[] · issue list" },
    { type: "P", label: "program", title: "commit", desc: "Write into memory and advance the progress bookmark; zero rows written counts as an error.", input: "reviewed drafts", output: "memory rows + watermark advanced" },
    { type: "P", label: "program", title: "metrics", desc: "Record only row counts, pass rate and the distribution of rejection reasons — never treat a model's self-reported score as a metric.", input: "commit results", output: "extraction_run readings" }
  ];

  var rail = document.querySelector("[data-rail]");
  if (rail) {
    var stageEls = Array.prototype.slice.call(rail.querySelectorAll("[data-stage]"));
    var pulse = rail.querySelector("[data-pulse]");
    var titleEl = document.querySelector("[data-sp-title]");
    var descEl = document.querySelector("[data-sp-desc]");
    var typeEl = document.querySelector("[data-sp-type]");
    var inEl = document.querySelector("[data-sp-in]");
    var outEl = document.querySelector("[data-sp-out]");
    var stagePanel = document.querySelector(".stagepanel");
    var playBtn = document.querySelector("[data-play]");
    var playing = !reduce;
    var idx = 0;
    var timer = 0;

    function render(i, animate) {
      idx = i;
      var s = STAGES[i];
      stageEls.forEach(function (el, k) {
        el.classList.toggle("is-active", k === i);
        el.classList.toggle("is-done", k < i);
      });
      pulse.style.left = ((i + 0.5) / STAGES.length) * 100 + "%";
      titleEl.textContent = s.title;
      descEl.textContent = s.desc;
      typeEl.textContent = s.label;
      typeEl.className = "chip " + (s.type === "L" ? "chip--brass" : s.type === "M" ? "chip--sage" : "chip--slate");
      inEl.textContent = s.input;
      outEl.textContent = s.output;
      if (animate && window.RC) window.RC.swapIn(stagePanel);
    }

    function stop() {
      playing = false;
      clearInterval(timer);
      playBtn.textContent = "▶ Play";
      playBtn.classList.remove("is-on");
      playBtn.setAttribute("aria-pressed", "false");
    }

    function start() {
      if (reduce) return;
      playing = true;
      playBtn.textContent = "⏸ Pause";
      playBtn.classList.add("is-on");
      playBtn.setAttribute("aria-pressed", "true");
      clearInterval(timer);
      timer = setInterval(function () {
        render((idx + 1) % STAGES.length, false);
      }, 2600);
    }

    stageEls.forEach(function (el, i) {
      el.addEventListener("click", function () {
        stop();
        render(i, true);
      });
    });

    playBtn.addEventListener("click", function () {
      if (playing) stop(); else start();
    });

    document.querySelectorAll("[data-step]").forEach(function (b) {
      b.addEventListener("click", function () {
        stop();
        var dir = b.getAttribute("data-step") === "next" ? 1 : -1;
        render((idx + dir + STAGES.length) % STAGES.length, true);
      });
    });

    render(0, false);
    if (playing) start();
  }

  /* ---------------- duty switch ---------------- */
  var colP = document.querySelector('[data-dutycol="p"]');
  var colL = document.querySelector('[data-dutycol="l"]');
  var verdict = document.querySelector("[data-verdict]");
  var TEXT = {
    both: '<b>Both sides:</b> code produces the exact position and provenance; the LLM makes one semantic judgement on that track. Every output can be traced back to <span class="code">seq</span> and a verbatim quote.',
    p: '<b>Code only:</b> it can locate, filter and check, but it cannot tell who "she" refers to or whether "because" marks causality. You get a pile of candidates that are precisely placed and semantically empty — signals that skip the model can narrow the range, never settle it.',
    l: '<b>Model only:</b> it can say what the words mean, but it loses anchors and provenance: one person may split into three, times are guessed, and pointers into the source text get invented. Without the code-supplied <span class="code">seq</span> as a baseline, review has nothing to judge against.'
  };

  document.querySelectorAll("[data-duty]").forEach(function (b) {
    b.addEventListener("click", function () {
      var mode = b.getAttribute("data-duty");
      document.querySelectorAll("[data-duty]").forEach(function (x) { x.classList.remove("is-on"); });
      b.classList.add("is-on");
      if (colP) colP.classList.toggle("is-off", mode === "l");
      if (colL) colL.classList.toggle("is-off", mode === "p");
      if (verdict) {
        verdict.innerHTML = TEXT[mode];
        if (window.RC) window.RC.swapIn(verdict);
      }
    });
  });

  /* ---------------- review gate ---------------- */
  var ISSUES = [
    { t: "Code check", m: 'Subject <code>Xiao Wang</code> is not in the evidence: it never appears in the quote for seq 12. Fix the converter (write the display name into the body) or change the subject to "deploy script".' },
    { t: "Code check", m: 'Time text <code>last week</code> is not in the quote. Time can only be converted from the original timestamps by code; the model cannot rewrite it.' },
    { t: "Model review", m: 'The draft says "because the Raspberry Pi would not run it", but the source only says "will not come up". Causality is an inference: mark it as inferred or leave it out of the long-term events — it cannot be committed as fact.' }
  ];

  var runBtn = document.querySelector("[data-run-gate]");
  var resetBtn = document.querySelector("[data-reset-gate]");
  var issuesEl = document.querySelector("[data-issues]");
  var gatecard = document.querySelector(".gatecard");
  var bar = document.querySelector("[data-gatebar]");
  var statusEl = document.querySelector("[data-gate-status]");
  var countEl = document.querySelector("[data-gate-count]");
  var timers = [];

  var HINT = "After it runs, each issue is listed by layer: code check / model review. The draft itself is never rewritten.";

  function resetGate() {
    timers.forEach(clearTimeout);
    if (gatecard) gatecard.classList.remove("is-checking");
    timers = [];
    if (issuesEl) issuesEl.innerHTML = '<p class="issues__hint">' + HINT + "</p>";
    if (bar) bar.className = "verdictbar";
    if (statusEl) statusEl.textContent = "Waiting to run";
    if (countEl) countEl.textContent = "0 issue";
  }

  function runGate() {
    resetGate();
    if (gatecard) gatecard.classList.add("is-checking");
    var hint = issuesEl.querySelector(".issues__hint");
    if (hint) hint.remove();
    if (statusEl) statusEl.textContent = "Code check running";
    ISSUES.forEach(function (it, i) {
      timers.push(setTimeout(function () {
        var d = document.createElement("div");
        d.className = "issue";
        var t = document.createElement("div");
        t.className = "issue__t";
        t.textContent = it.t;
        var m = document.createElement("div");
        m.className = "issue__m";
        m.innerHTML = it.m;
        d.appendChild(t);
        d.appendChild(m);
        issuesEl.appendChild(d);
        requestAnimationFrame(function () { d.classList.add("is-in"); });
        countEl.textContent = (i + 1) + " issue";
        statusEl.textContent = i < 2 ? "Code check running" : "Model review running";
        if (i === ISSUES.length - 1) {
          timers.push(setTimeout(function () {
            if (gatecard) gatecard.classList.remove("is-checking");
            bar.className = "verdictbar is-reject";
            statusEl.textContent = "Rejected · draft not rewritten";
            countEl.textContent = ISSUES.length + " issue · 3 layers";
          }, 600));
        }
      }, 420 * (i + 1)));
    });
  }

  if (runBtn) runBtn.addEventListener("click", runGate);
  if (resetBtn) resetBtn.addEventListener("click", resetGate);
})();
