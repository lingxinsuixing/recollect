/* recollect — pipeline: 段落播放、职责切换、审核闸 */
(function () {
  "use strict";

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------------- 流水线 ---------------- */
  var STAGES = [
    { type: "P", label: "program", title: "ingest", desc: "把 agent 自己的会话格式翻译成统一格式；不认识的字段、扫到的秘密，直接拒收。", input: "session.json（agent 私有格式）→ 转换器", output: "events[] · 每条带 seq / speaker_id / ts_ms / text" },
    { type: "P", label: "program", title: "sanitize", desc: "脱敏：把敏感信息换成不可逆的替身；原文不删不改。", input: "events[]", output: "events[] + pii 命中报告" },
    { type: "P", label: "program", title: "locate", desc: "序号、时间、谁说的、工具调用、字位置，全由程序确定；模型不碰这些。", input: "events[]", output: "locators[] · turn_uid / span / speaker_id / ts_ms" },
    { type: "P", label: "program", title: "candidates", desc: "先把「提到了谁」列出来，再剔除明显不是人的东西：工具名、UUID、路径、纯方法名。", input: "locators[] + text", output: "candidates[] · 每处出现带 span" },
    { type: "L", label: "llm", title: "naming", desc: "给实体起名、合并别名；名字一样但其实是两个人的，也在这里分开，不靠字面相似度。", input: "candidates[] + 上下文", output: "anchors[] + alias[]（只带 seq 与引文）" },
    { type: "P", label: "program", title: "expand mentions", desc: "把每处提及都挂到对应的锚上，出现的位置逐条保留，不合并成一条。", input: "anchors[] + candidates[]", output: "mention[] · anchor × span" },
    { type: "L", label: "llm", title: "disambiguate", desc: "代词和歧义只判一个：这一次的「她」是谁。判不了就留空，不猜、不填默认值。", input: "flagged mentions[]", output: "verdict[] 或空" },
    { type: "L", label: "llm", title: "facts", desc: "事实抽取，一类一次。每条只带 seq 与逐字引文；没有内容就返回空。", input: "turn 文本 + 锚", output: "{ seq, quote, facts[] }" },
    { type: "L", label: "llm", title: "events / relations / states", desc: "事件、关系、状态分开抽。回复链、调用与结果这类关系由程序先连好，模型只补语义上的关系。", input: "facts[] + 结构边", output: "episode[] / relation[] / state_assertion[]" },
    { type: "P", label: "program", title: "validate", desc: "程序检查：主语、时间、值都得在引文里找得到；找不到就带着原因退回。", input: "全部草稿", output: "passed[] + issues[]" },
    { type: "L", label: "llm", title: "review", desc: "模型审核单独走：只标问题，不改写、不重抽；问题归到转换器 / 程序 / 提示词 / 模型。", input: "passed[]", output: "review_decision[] · issue 列表" },
    { type: "P", label: "program", title: "commit", desc: "写进记忆、把进度书签往前挪；写零行就算错误。", input: "过审草稿", output: "memory 行 + watermark 前进" },
    { type: "P", label: "program", title: "metrics", desc: "只记行数、过审率、退回原因分布——不把模型自报的分数当指标。", input: "提交结果", output: "extraction_run 读数" }
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
      playBtn.textContent = "▶ 播放";
      playBtn.classList.remove("is-on");
      playBtn.setAttribute("aria-pressed", "false");
    }

    function start() {
      if (reduce) return;
      playing = true;
      playBtn.textContent = "⏸ 暂停";
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

  /* ---------------- 职责切换 ---------------- */
  var colP = document.querySelector('[data-dutycol="p"]');
  var colL = document.querySelector('[data-dutycol="l"]');
  var verdict = document.querySelector("[data-verdict]");
  var TEXT = {
    both: '<b>两边都在：</b>程序给出确定的位置与出处，LLM 只在这条轨道上做一次语义判断。每条输出都能回到 <span class="code">seq</span> 与逐字引文。',
    p: '<b>只留程序：</b>能定位、能过滤、能检查，但判不了「她」指的是谁、「因为」是不是因果。系统会得到一堆位置准确、意义为空的候选——不用模型的信号只能用来缩小范围，不能下结论。',
    l: '<b>只留模型：</b>能说出意思，但失去锚和出处：同一个人可能被拆成三个，时间靠猜，指向原文的编号也会被编出来。没有程序给的 <span class="code">seq</span> 做基准，审核也无从判断。'
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

  /* ---------------- 审核闸 ---------------- */
  var ISSUES = [
    { t: "程序检查", m: '主语 <code>小王</code> 不在证据里：seq 12 的引文中没有出现该主语。改转换器（把显示名补进正文），或把主语改成「部署脚本」。' },
    { t: "程序检查", m: '时间文本 <code>上周</code> 不在引文里。时间只能由程序按原始时间换算，模型不能改写。' },
    { t: "模型审核", m: '草稿写了「因为树莓派跑不起来」，原文只说「直接起不来」。因果是推断，要么标成「推断」，要么别写进长期事件，不能当事实提交。' }
  ];

  var runBtn = document.querySelector("[data-run-gate]");
  var resetBtn = document.querySelector("[data-reset-gate]");
  var issuesEl = document.querySelector("[data-issues]");
  var gatecard = document.querySelector(".gatecard");
  var bar = document.querySelector("[data-gatebar]");
  var statusEl = document.querySelector("[data-gate-status]");
  var countEl = document.querySelector("[data-gate-count]");
  var timers = [];

  var HINT = "运行后，每条问题会按「程序检查 / 模型审核」分层列出；草稿本身不会被改写。";

  function resetGate() {
    timers.forEach(clearTimeout);
    if (gatecard) gatecard.classList.remove("is-checking");
    timers = [];
    if (issuesEl) issuesEl.innerHTML = '<p class="issues__hint">' + HINT + "</p>";
    if (bar) bar.className = "verdictbar";
    if (statusEl) statusEl.textContent = "等待运行";
    if (countEl) countEl.textContent = "0 issue";
  }

  function runGate() {
    resetGate();
    if (gatecard) gatecard.classList.add("is-checking");
    var hint = issuesEl.querySelector(".issues__hint");
    if (hint) hint.remove();
    if (statusEl) statusEl.textContent = "程序检查中";
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
        statusEl.textContent = i < 2 ? "程序检查中" : "模型审核中";
        if (i === ISSUES.length - 1) {
          timers.push(setTimeout(function () {
            if (gatecard) gatecard.classList.remove("is-checking");
            bar.className = "verdictbar is-reject";
            statusEl.textContent = "退回 · 不改写草稿";
            countEl.textContent = ISSUES.length + " issue · 3 层";
          }, 600));
        }
      }, 420 * (i + 1)));
    });
  }

  if (runBtn) runBtn.addEventListener("click", runGate);
  if (resetBtn) resetBtn.addEventListener("click", resetGate);
})();
