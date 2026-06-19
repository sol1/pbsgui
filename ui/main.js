// pbsgui frontend: manage backup jobs, run them, and browse/restore snapshots.
// Uses the global Tauri API (app.withGlobalTauri = true), so no JS build step.

const { invoke, Channel } = window.__TAURI__.core;
const el = (id) => document.getElementById(id);

let editing = null; // job being edited, or null for a new job
let currentSources = [];
let browseJobId = null;
let snapshotTime = null;

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

function formatBytes(n) {
  if (n === null || n === undefined) return "?";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
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
  el("browse-view").classList.toggle("hidden", which !== "browse");
  el("tab-jobs").classList.toggle("active", which === "jobs" || which === "editor");
  el("tab-browse").classList.toggle("active", which === "browse");
}

async function checkEngine() {
  try {
    await invoke("engine_ping");
    el("engine-status").textContent = "connected";
  } catch (err) {
    el("engine-status").textContent = `unavailable (${err})`;
  }
}

// --- Jobs ---------------------------------------------------------------

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

// --- Run / restore output (shared) --------------------------------------

function setProgress(fraction, label) {
  el("progress-bar").style.width = `${Math.round(fraction * 100)}%`;
  el("progress-label").textContent = label;
}

function appendLog(line) {
  const log = el("log");
  log.textContent += line + "\n";
  log.scrollTop = log.scrollHeight;
}

async function streamRun(title, command, args) {
  el("run-title").textContent = title;
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
    await invoke(command, { ...args, onEvent: channel });
  } catch (err) {
    appendLog("error: " + err);
    setProgress(0, "failed");
  }
}

function runJob(job) {
  streamRun("Running: " + job.name, "run_job", { id: job.id });
}

// --- Browse & restore ---------------------------------------------------

async function populateBrowseJobs() {
  let jobs = [];
  try {
    jobs = await invoke("list_jobs");
  } catch (err) {
    /* engine offline; leave empty */
  }
  const sel = el("browse-job");
  sel.innerHTML = "";
  for (const job of jobs) {
    const opt = document.createElement("option");
    opt.value = job.id;
    opt.textContent = job.name;
    sel.append(opt);
  }
}

async function loadSnapshots() {
  const jobId = el("browse-job").value;
  if (!jobId) return alert("Create a job first, then browse its snapshots.");
  el("files-panel").classList.add("hidden");
  el("snapshots-list").innerHTML = '<div class="muted">loading...</div>';
  let snaps;
  try {
    snaps = await invoke("list_snapshots", { jobId });
  } catch (err) {
    el("snapshots-list").innerHTML = `<div class="placeholder">error: ${escapeHtml(err)}</div>`;
    return;
  }
  const list = el("snapshots-list");
  list.innerHTML = "";
  if (!snaps.length) {
    list.innerHTML = '<div class="placeholder">No snapshots in this group yet.</div>';
    return;
  }
  snaps.sort((a, b) => b.backup_time - a.backup_time);
  for (const snap of snaps) {
    const row = document.createElement("div");
    row.className = "snap-row";
    row.innerHTML =
      `<span class="snap-time">${escapeHtml(new Date(snap.backup_time * 1000).toLocaleString())}</span>` +
      `<span class="snap-size muted">${escapeHtml(formatBytes(snap.size))}</span>`;
    row.onclick = () => loadFiles(jobId, snap.backup_time, snap.backup_time);
    list.append(row);
  }
}

async function loadFiles(jobId, backupTime, label) {
  browseJobId = jobId;
  snapshotTime = backupTime;
  el("files-panel").classList.remove("hidden");
  el("files-title").textContent =
    "Files in " + new Date(label * 1000).toLocaleString();
  el("files-list").innerHTML = '<div class="muted">loading (downloading archive)...</div>';
  let files;
  try {
    files = await invoke("list_files", { jobId, backupTime });
  } catch (err) {
    el("files-list").innerHTML = `<div class="placeholder">error: ${escapeHtml(err)}</div>`;
    return;
  }
  const list = el("files-list");
  list.innerHTML = "";
  if (!files.length) {
    list.innerHTML = '<div class="muted">no files</div>';
    return;
  }
  for (const file of files) {
    const row = document.createElement("label");
    row.className = "file-row";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.value = file.path;
    const name = document.createElement("span");
    name.textContent = file.path;
    const size = document.createElement("span");
    size.className = "muted";
    size.textContent = formatBytes(file.size);
    row.append(cb, name, size);
    list.append(row);
  }
}

async function doRestore(all) {
  if (!browseJobId || snapshotTime === null) return;
  let files = null;
  if (!all) {
    files = Array.from(el("files-list").querySelectorAll("input:checked")).map((c) => c.value);
    if (!files.length) return alert("Select at least one file, or use Restore all.");
  }
  const destination = await invoke("pick_destination");
  if (!destination) return;
  streamRun(`Restoring to ${destination}`, "restore", {
    jobId: browseJobId,
    backupTime: snapshotTime,
    files,
    destination,
  });
}

window.addEventListener("DOMContentLoaded", () => {
  el("tab-jobs").onclick = () => showView("jobs");
  el("tab-browse").onclick = () => {
    showView("browse");
    populateBrowseJobs();
  };
  el("new-job").onclick = () => openEditor(null);
  el("job-form").addEventListener("submit", saveJob);
  el("cancel-edit").onclick = () => showView("jobs");
  el("add-folders").onclick = async () =>
    renderSources([...currentSources, ...(await invoke("pick_folders"))]);
  el("add-files").onclick = async () =>
    renderSources([...currentSources, ...(await invoke("pick_files"))]);
  el("f-schedule-kind").onchange = updateScheduleFields;
  el("load-snapshots").onclick = loadSnapshots;
  el("restore-all").onclick = () => doRestore(true);
  el("restore-selected").onclick = () => doRestore(false);
  checkEngine();
  loadJobs();
});
