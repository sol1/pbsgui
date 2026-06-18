// pbsgui frontend: manage backup jobs, pick sources, run with live progress.
// Uses the global Tauri API (app.withGlobalTauri = true), so no JS build step.

const { invoke, Channel } = window.__TAURI__.core;
const el = (id) => document.getElementById(id);

let editing = null; // job being edited, or null for a new job
let currentSources = [];

function escapeHtml(s) {
  return String(s).replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
  );
}

function mkbtn(text, cls, fn) {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = text;
  if (cls) b.className = cls;
  b.onclick = fn;
  return b;
}

function scheduleSummary(s) {
  if (!s) return "";
  if (s.kind === "interval") return `every ${s.minutes} min`;
  if (s.kind === "daily")
    return `daily ${String(s.hour).padStart(2, "0")}:${String(s.minute).padStart(2, "0")}`;
  return "manual";
}

function showView(which) {
  el("jobs-view").classList.toggle("hidden", which !== "jobs");
  el("editor").classList.toggle("hidden", which !== "editor");
}

async function checkEngine() {
  try {
    await invoke("engine_ping");
    el("engine-status").textContent = "connected";
  } catch (err) {
    el("engine-status").textContent = `unavailable (${err})`;
  }
}

async function loadJobs() {
  let jobs;
  try {
    jobs = await invoke("list_jobs");
  } catch (err) {
    el("jobs-list").innerHTML = `<div class="placeholder">error: ${escapeHtml(err)}</div>`;
    return;
  }
  const list = el("jobs-list");
  list.innerHTML = "";
  if (!jobs.length) {
    list.innerHTML = '<div class="placeholder">No jobs yet. Click "New job".</div>';
    return;
  }
  for (const job of jobs) {
    const row = document.createElement("div");
    row.className = "job-row";
    const last = job.last_run ? new Date(job.last_run * 1000).toLocaleString() : "never";
    const status = job.last_status ? ` (${job.last_status})` : "";
    const main = document.createElement("div");
    main.className = "job-main";
    main.innerHTML =
      `<div class="job-name">${escapeHtml(job.name)}</div>` +
      `<div class="job-meta">${escapeHtml(scheduleSummary(job.schedule))} · ` +
      `${job.sources.length} source(s) · last: ${escapeHtml(last)}${escapeHtml(status)}</div>`;
    const actions = document.createElement("div");
    actions.className = "job-actions";
    actions.append(
      mkbtn("Run", "primary", () => runJob(job)),
      mkbtn("Edit", "", () => openEditor(job)),
      mkbtn("Delete", "", () => deleteJob(job)),
    );
    row.append(main, actions);
    list.append(row);
  }
}

function renderSources(list) {
  currentSources = [...list];
  const c = el("sources");
  c.innerHTML = "";
  if (!currentSources.length) {
    c.innerHTML = '<div class="muted">none selected</div>';
    return;
  }
  currentSources.forEach((p, i) => {
    const row = document.createElement("div");
    row.className = "source-row";
    const span = document.createElement("span");
    span.textContent = p;
    row.append(
      span,
      mkbtn("remove", "", () => {
        currentSources.splice(i, 1);
        renderSources(currentSources);
      }),
    );
    c.append(row);
  });
}

function updateScheduleFields() {
  const k = el("f-schedule-kind").value;
  el("interval-fields").classList.toggle("hidden", k !== "interval");
  el("daily-fields").classList.toggle("hidden", k !== "daily");
}

function openEditor(job) {
  editing = job || null;
  el("editor-title").textContent = job ? "Edit job" : "New job";
  el("f-name").value = job?.name || "";
  el("f-repository").value = job?.destination?.repository || "";
  el("f-secret").value = "";
  el("secret-note").textContent = job ? "leave blank to keep the saved secret" : "";
  el("f-fingerprint").value = job?.destination?.fingerprint || "";
  el("f-backup-id").value = job?.destination?.backup_id || "pbsgui-host";
  renderSources(job?.sources || []);
  el("f-excludes").value = (job?.excludes || []).join("\n");
  const s = job?.schedule || { kind: "manual" };
  el("f-schedule-kind").value = s.kind;
  el("f-interval").value = s.kind === "interval" ? s.minutes : 60;
  el("f-daily-time").value =
    s.kind === "daily"
      ? `${String(s.hour).padStart(2, "0")}:${String(s.minute).padStart(2, "0")}`
      : "02:00";
  updateScheduleFields();
  showView("editor");
}

function gatherSchedule() {
  const k = el("f-schedule-kind").value;
  if (k === "interval") {
    return { kind: "interval", minutes: parseInt(el("f-interval").value, 10) || 30 };
  }
  if (k === "daily") {
    const [h, m] = el("f-daily-time").value.split(":").map((x) => parseInt(x, 10) || 0);
    return { kind: "daily", hour: h, minute: m };
  }
  return { kind: "manual" };
}

function gatherJob() {
  return {
    id: editing?.id || crypto.randomUUID(),
    name: el("f-name").value.trim(),
    destination: {
      repository: el("f-repository").value.trim(),
      fingerprint: el("f-fingerprint").value.trim(),
      backup_id: el("f-backup-id").value.trim(),
    },
    sources: currentSources,
    excludes: el("f-excludes")
      .value.split("\n")
      .map((s) => s.trim())
      .filter(Boolean),
    schedule: gatherSchedule(),
    last_run: editing?.last_run ?? null,
    last_status: editing?.last_status ?? null,
  };
}

async function saveJob(event) {
  event.preventDefault();
  const job = gatherJob();
  if (!job.name) return alert("Name is required");
  if (!job.sources.length) return alert("Add at least one source");
  const secretVal = el("f-secret").value;
  const secret = secretVal ? secretVal : null;
  if (!editing && !secret) return alert("A token secret is required for a new job");
  try {
    await invoke("save_job", { job, secret });
  } catch (err) {
    return alert("save failed: " + err);
  }
  showView("jobs");
  loadJobs();
}

async function deleteJob(job) {
  if (!confirm(`Delete job "${job.name}"?`)) return;
  try {
    await invoke("delete_job", { id: job.id });
  } catch (err) {
    alert("delete failed: " + err);
  }
  loadJobs();
}

function setProgress(fraction, label) {
  el("progress-bar").style.width = `${Math.round(fraction * 100)}%`;
  el("progress-label").textContent = label;
}

function appendLog(line) {
  const log = el("log");
  log.textContent += line + "\n";
  log.scrollTop = log.scrollHeight;
}

async function runJob(job) {
  el("run-title").textContent = "Running: " + job.name;
  el("run").classList.remove("hidden");
  el("log").textContent = "";
  setProgress(0, "starting...");

  const channel = new Channel();
  channel.onmessage = (reply) => {
    switch (reply.reply) {
      case "accepted":
        appendLog("accepted: " + reply.job_id);
        break;
      case "log":
        appendLog(reply.line);
        break;
      case "progress":
        setProgress(reply.fraction, reply.message);
        break;
      case "finished":
        setProgress(reply.success ? 1 : 0, reply.message);
        appendLog((reply.success ? "OK: " : "FAILED: ") + reply.message);
        loadJobs();
        break;
      case "error":
        appendLog("error: " + reply.message);
        break;
    }
  };

  try {
    await invoke("run_job", { id: job.id, onEvent: channel });
  } catch (err) {
    appendLog("error: " + err);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  el("new-job").onclick = () => openEditor(null);
  el("job-form").addEventListener("submit", saveJob);
  el("cancel-edit").onclick = () => showView("jobs");
  el("add-folders").onclick = async () =>
    renderSources([...currentSources, ...(await invoke("pick_folders"))]);
  el("add-files").onclick = async () =>
    renderSources([...currentSources, ...(await invoke("pick_files"))]);
  el("f-schedule-kind").onchange = updateScheduleFields;
  checkEngine();
  loadJobs();
});
