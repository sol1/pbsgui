// pbsgui frontend: manage backup jobs, run them, and browse/restore snapshots.
// Uses the global Tauri API (app.withGlobalTauri = true), so no JS build step.

const { invoke, Channel } = window.__TAURI__.core;
const el = (id) => document.getElementById(id);

let editing = null; // job being edited, or null for a new job
let currentSources = [];
let browseJobId = null;
let snapshotTime = null;
let backupIdTouched = false; // has the user edited the Backup id directly?
let wizStep = 0; // current job-wizard step
let sqlConnCache = []; // saved SQL connections, for the wizard's source step

// Derive a PBS-safe snapshot group id from a job name.
function slug(s) {
  const out = s
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[-.]+|[-.]+$/g, "")
    .slice(0, 60);
  return out;
}

function escapeHtml(s) {
  return String(s).replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
  );
}

function loadingHtml(text) {
  return `<div class="loading"><span class="spinner"></span>${escapeHtml(text)}</div>`;
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

function sourceSummary(job) {
  const s = job.source || {};
  if (s.type === "sql") return `SQL: ${(s.databases || []).length} db(s)`;
  return `Files: ${(s.sources || []).length} source(s)`;
}

function destSummary(job) {
  const d = job.destination || {};
  return d.type === "folder" ? `folder ${d.path}` : "PBS";
}

// Browse/restore currently supports file backups to PBS.
function isBrowsable(job) {
  return job.source?.type === "files" && job.destination?.type === "pbs";
}

function showView(which) {
  el("jobs-view").classList.toggle("hidden", which !== "jobs");
  el("editor").classList.toggle("hidden", which !== "editor");
  el("browse-view").classList.toggle("hidden", which !== "browse");
  el("sql-view").classList.toggle("hidden", which !== "sql");
  el("pbs-view").classList.toggle("hidden", which !== "pbs");
  el("tab-jobs").classList.toggle("active", which === "jobs" || which === "editor");
  el("tab-browse").classList.toggle("active", which === "browse");
  el("tab-sql").classList.toggle("active", which === "sql");
  el("tab-pbs").classList.toggle("active", which === "pbs");
}

async function checkEngine() {
  try {
    const ok = await invoke("engine_status");
    el("engine-status").textContent = ok ? "running" : "not reachable";
  } catch (err) {
    el("engine-status").textContent = "not reachable";
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
      `<div class="job-meta">${escapeHtml(sourceSummary(job))} → ${escapeHtml(destSummary(job))} · ` +
      `${escapeHtml(scheduleSummary(job.schedule))} · last: ${escapeHtml(last)}${escapeHtml(status)}</div>`;
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

const WIZ_STEPS = ["source", "dest", "schedule"];

function showWizStep(n) {
  wizStep = Math.max(0, Math.min(n, WIZ_STEPS.length - 1));
  WIZ_STEPS.forEach((name, i) => {
    el(`wiz-step-${name}`).classList.toggle("hidden", i !== wizStep);
    el(`wiz-tab-${name}`).classList.toggle("wiz-active", i === wizStep);
  });
  el("wiz-back").disabled = wizStep === 0;
  const last = wizStep === WIZ_STEPS.length - 1;
  el("wiz-next").classList.toggle("hidden", last);
  el("wiz-save").classList.toggle("hidden", !last);
}

function updateSourceType() {
  const sql = el("f-source-type").value === "sql";
  el("src-sql").classList.toggle("hidden", !sql);
  el("src-files").classList.toggle("hidden", sql);
}

function updateDestType() {
  const folder = el("f-dest-type").value === "folder";
  el("dest-folder").classList.toggle("hidden", !folder);
  el("dest-pbs").classList.toggle("hidden", folder);
}

function renderDbCheckboxes(databases, checked) {
  const checkedSet = new Set(checked || []);
  const c = el("sql-db-pick");
  if (!databases.length) {
    c.innerHTML = '<div class="muted">no databases</div>';
    return;
  }
  c.innerHTML = "";
  for (const name of databases) {
    const row = document.createElement("label");
    row.className = "file-row";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.value = name;
    cb.checked = checkedSet.has(name);
    const span = document.createElement("span");
    span.textContent = name;
    row.append(cb, span);
    c.append(row);
  }
}

async function loadDatabasesForConn() {
  const conn = sqlConnCache.find((c) => c.id === el("f-sql-conn").value);
  if (!conn) return alert("Pick a SQL connection first.");
  const checked = Array.from(el("sql-db-pick").querySelectorAll("input:checked")).map((c) => c.value);
  el("sql-db-pick").innerHTML = loadingHtml("loading databases...");
  try {
    const probe = await invoke("probe_sql", {
      server: conn.server,
      port: conn.port ?? null,
      auth: conn.auth,
      password: null,
    });
    renderDbCheckboxes(
      probe.databases.map((d) => d.name),
      checked,
    );
  } catch (err) {
    el("sql-db-pick").innerHTML = `<div class="placeholder">could not load databases: ${escapeHtml(err)}</div>`;
  }
}

async function populateJobPickers(job) {
  try {
    sqlConnCache = await invoke("list_sql_connections");
  } catch (err) {
    sqlConnCache = [];
  }
  const connSel = el("f-sql-conn");
  connSel.innerHTML = "";
  for (const c of sqlConnCache) {
    const o = document.createElement("option");
    o.value = c.id;
    o.textContent = `${c.name} (${c.server})`;
    connSel.append(o);
  }
  let servers = [];
  try {
    servers = await invoke("list_pbs_servers");
  } catch (err) {
    /* engine offline */
  }
  const pbsSel = el("f-pbs-server");
  pbsSel.innerHTML = "";
  for (const s of servers) {
    const o = document.createElement("option");
    o.value = s.id;
    o.textContent = `${s.name} (${s.repository})`;
    pbsSel.append(o);
  }
  if (job?.source?.type === "sql" && job.source.connection_id) connSel.value = job.source.connection_id;
  if (job?.destination?.type === "pbs" && job.destination.server_id) {
    pbsSel.value = job.destination.server_id;
  }
}

async function openEditor(job) {
  editing = job || null;
  el("editor-title").textContent = job ? "Edit job" : "New job";
  el("f-name").value = job?.name || "";
  await populateJobPickers(job);

  const source = job?.source || { type: "files" };
  el("f-source-type").value = source.type;
  renderSources(source.type === "files" ? source.sources || [] : []);
  el("f-excludes").value = (source.type === "files" ? source.excludes || [] : []).join("\n");
  el("f-change-detection").checked = source.type === "files" ? !!source.change_detection : false;
  if (source.type === "sql") {
    renderDbCheckboxes(source.databases || [], source.databases || []);
  } else {
    el("sql-db-pick").innerHTML = '<div class="muted">Pick a connection and load its databases.</div>';
  }
  updateSourceType();

  const dest = job?.destination || { type: "pbs" };
  el("f-dest-type").value = dest.type;
  el("f-backup-id").value = dest.type === "pbs" ? dest.backup_id || "" : "";
  el("f-folder").value = dest.type === "folder" ? dest.path || "" : "";
  backupIdTouched = !!(dest.type === "pbs" && dest.backup_id);
  updateDestType();

  const s = job?.schedule || { kind: "manual" };
  el("f-schedule-kind").value = s.kind;
  el("f-interval").value = s.kind === "interval" ? s.minutes : 60;
  el("f-daily-time").value =
    s.kind === "daily"
      ? `${String(s.hour).padStart(2, "0")}:${String(s.minute).padStart(2, "0")}`
      : "02:00";
  el("f-pre-script").value = job?.pre_script || "";
  el("f-post-script").value = job?.post_script || "";
  updateScheduleFields();
  showWizStep(0);
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

function gatherSource() {
  if (el("f-source-type").value === "sql") {
    const databases = Array.from(el("sql-db-pick").querySelectorAll("input:checked")).map((c) => c.value);
    return {
      type: "sql",
      connection_id: el("f-sql-conn").value,
      databases,
      backup_type: "full",
      copy_only: true,
    };
  }
  return {
    type: "files",
    sources: currentSources,
    excludes: el("f-excludes")
      .value.split("\n")
      .map((s) => s.trim())
      .filter(Boolean),
    change_detection: el("f-change-detection").checked,
  };
}

function gatherDestination() {
  if (el("f-dest-type").value === "folder") {
    return { type: "folder", path: el("f-folder").value.trim() };
  }
  return { type: "pbs", server_id: el("f-pbs-server").value, backup_id: el("f-backup-id").value.trim() };
}

function gatherJob() {
  return {
    id: editing?.id || crypto.randomUUID(),
    name: el("f-name").value.trim(),
    source: gatherSource(),
    destination: gatherDestination(),
    schedule: gatherSchedule(),
    pre_script: el("f-pre-script").value.trim() || null,
    post_script: el("f-post-script").value.trim() || null,
    last_run: editing?.last_run ?? null,
    last_status: editing?.last_status ?? null,
  };
}

async function saveJob() {
  const job = gatherJob();
  if (!job.name) return alert("Name is required");
  if (job.source.type === "files" && !job.source.sources.length) {
    return alert("Add at least one source");
  }
  if (job.source.type === "sql") {
    if (!job.source.connection_id) return alert("Pick a SQL connection");
    if (!job.source.databases.length) return alert("Select at least one database");
  }
  if (job.destination.type === "pbs") {
    if (!job.destination.server_id) return alert("Pick a PBS server (add one in the PBS servers tab)");
    if (!job.destination.backup_id) return alert("Backup id is required");
  } else if (!job.destination.path) {
    return alert("Folder path is required");
  }
  if (job.source.type === "files" && job.destination.type === "folder") {
    return alert("Backing up files to a folder is not supported yet; choose PBS.");
  }
  try {
    await invoke("save_job", { job });
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
  for (const job of jobs.filter(isBrowsable)) {
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
  const list = el("snapshots-list");
  // Only show the spinner if the load is slow, so fast loads don't flash.
  const spinner = setTimeout(() => {
    list.innerHTML = loadingHtml("loading snapshots...");
  }, 200);
  let snaps;
  try {
    snaps = await invoke("list_snapshots", { jobId });
  } catch (err) {
    clearTimeout(spinner);
    list.innerHTML = `<div class="placeholder">error: ${escapeHtml(err)}</div>`;
    return;
  }
  clearTimeout(spinner);
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
  const list = el("files-list");
  // Only show the spinner if the load is slow, so fast loads don't flash.
  const spinner = setTimeout(() => {
    list.innerHTML = loadingHtml("loading files...");
  }, 200);
  let files;
  try {
    files = await invoke("list_files", { jobId, backupTime });
  } catch (err) {
    clearTimeout(spinner);
    if (snapshotTime !== backupTime) return; // a newer selection superseded this
    list.innerHTML = `<div class="placeholder">error: ${escapeHtml(err)}</div>`;
    return;
  }
  clearTimeout(spinner);
  if (snapshotTime !== backupTime) return; // a newer selection superseded this
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

// --- SQL Servers --------------------------------------------------------

const authModeLabel = { windows_only: "Windows auth", mixed: "Mixed auth", unknown: "" };
const sourceLabel = {
  local_registry: "local",
  browser: "browser",
  network_scan: "network",
  active_directory: "AD",
};

function recoveryBadge(model) {
  const cls = model === "SIMPLE" ? "badge" : "badge badge-ok";
  return `<span class="${cls}">${escapeHtml(model)}</span>`;
}

function topologyLabel(t) {
  if (!t) return "";
  if (t.topology === "failover_cluster_instance") return `FCI (node ${escapeHtml(t.current_node)})`;
  if (t.topology === "availability_group") {
    const pref = t.is_preferred_backup_replica ? " · preferred backup" : "";
    return `Always On ${escapeHtml(t.group_name)} (${escapeHtml(t.role)})${pref}`;
  }
  return "Standalone";
}

// tempdb cannot be backed up; only ONLINE user/system databases get a button.
function canBackup(db) {
  return db.state === "ONLINE" && db.name.toLowerCase() !== "tempdb";
}

function renderSqlDatabases(databases) {
  if (!databases || !databases.length) return '<div class="muted">no databases</div>';
  return databases
    .map((db) => {
      const wait =
        db.log_reuse_wait && db.log_reuse_wait !== "NOTHING"
          ? ` · log wait: ${escapeHtml(db.log_reuse_wait)}`
          : "";
      const ag = db.in_availability_group ? " · in AG" : "";
      const db64 = escapeHtml(db.name);
      const button = canBackup(db)
        ? `<span class="spacer"></span>` +
          `<button type="button" class="sql-db-pbs" data-db="${db64}">To PBS</button>` +
          `<button type="button" class="sql-db-backup" data-db="${db64}">To file</button>`
        : "";
      return (
        `<div class="sql-db"><span class="sql-db-name">${escapeHtml(db.name)}</span>` +
        `${recoveryBadge(db.recovery_model)}` +
        `<span class="muted">${escapeHtml(db.state)}${ag}${wait}</span>${button}</div>`
      );
    })
    .join("");
}

// Back up a database over VDI to a local .bak file (validation step before PBS).
async function backupDatabase(inst, dbName) {
  const path = await invoke("pick_save_file", { defaultName: `${dbName}.bak` });
  if (!path) return;
  streamRun(`Backing up ${dbName}`, "backup_sql_to_file", {
    server: inst.server,
    port: inst.port ?? null,
    auth: { kind: "integrated" },
    password: null,
    database: dbName,
    outputPath: path,
  });
}

// Back up a database over VDI straight to PBS, sending it to a saved PBS server.
async function backupDatabaseToPbs(inst, dbName) {
  const pbsServerId = el("sql-pbs-server").value;
  if (!pbsServerId) {
    return alert("Add a PBS server first (the PBS servers tab), then pick it as the target.");
  }
  streamRun(`Backing up ${dbName} to PBS`, "backup_sql_to_pbs", {
    server: inst.server,
    port: inst.port ?? null,
    auth: { kind: "integrated" },
    password: null,
    database: dbName,
    pbsServerId,
    backupId: `mssql-${slug(dbName)}`,
  });
}

// Populate the PBS target dropdown from saved PBS servers.
async function populatePbsServers() {
  let servers = [];
  try {
    servers = await invoke("list_pbs_servers");
  } catch (err) {
    /* engine offline; leave empty */
  }
  const sel = el("sql-pbs-server");
  const previous = sel.value;
  sel.innerHTML = "";
  if (!servers.length) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "no PBS servers yet";
    sel.append(opt);
    return;
  }
  for (const s of servers) {
    const opt = document.createElement("option");
    opt.value = s.id;
    opt.textContent = `${s.name} (${s.repository})`;
    sel.append(opt);
  }
  if (previous) sel.value = previous;
}

// --- Saved SQL connections ---------------------------------------------

async function saveSqlConnection(inst) {
  const name = prompt("Name for this SQL connection:", inst.server);
  if (!name) return;
  try {
    await invoke("save_sql_connection", {
      connection: {
        id: crypto.randomUUID(),
        name,
        server: inst.server,
        port: inst.port ?? null,
        auth: { kind: "integrated" },
      },
      secret: null,
    });
    loadSqlConnections();
  } catch (err) {
    alert("save failed: " + err);
  }
}

async function loadSqlConnections() {
  let conns = [];
  try {
    conns = await invoke("list_sql_connections");
  } catch (err) {
    /* engine offline */
  }
  const list = el("sql-conns");
  list.innerHTML = "";
  if (!conns.length) {
    list.innerHTML = '<div class="muted">No saved connections yet.</div>';
    return;
  }
  for (const conn of conns) {
    const row = document.createElement("div");
    row.className = "job-row";
    const main = document.createElement("div");
    main.className = "job-main";
    main.innerHTML =
      `<div class="job-name">${escapeHtml(conn.name)}</div>` +
      `<div class="job-meta">${escapeHtml(conn.server)} · ${escapeHtml(conn.auth.kind)}</div>`;
    const actions = document.createElement("div");
    actions.className = "job-actions";
    actions.append(
      mkbtn("Delete", "", async () => {
        if (!confirm(`Delete connection "${conn.name}"?`)) return;
        try {
          await invoke("delete_sql_connection", { id: conn.id });
        } catch (err) {
          alert("delete failed: " + err);
        }
        loadSqlConnections();
      }),
    );
    row.append(main, actions);
    list.append(row);
  }
}

// --- PBS servers --------------------------------------------------------

let editingPbs = null; // id of the server being edited, or null

function resetPbsForm() {
  editingPbs = null;
  el("pbs-name").value = "";
  el("pbs-repository").value = "";
  el("pbs-fingerprint").value = "";
  el("pbs-secret").value = "";
  el("pbs-secret-note").textContent = "";
}

function editPbsServer(server) {
  editingPbs = server.id;
  el("pbs-name").value = server.name;
  el("pbs-repository").value = server.repository;
  el("pbs-fingerprint").value = server.fingerprint;
  el("pbs-secret").value = "";
  el("pbs-secret-note").textContent = "leave blank to keep the saved secret";
}

async function savePbsServer(event) {
  event.preventDefault();
  const server = {
    id: editingPbs || crypto.randomUUID(),
    name: el("pbs-name").value.trim(),
    repository: el("pbs-repository").value.trim(),
    fingerprint: el("pbs-fingerprint").value.trim(),
  };
  if (!server.name || !server.repository || !server.fingerprint) {
    return alert("Name, repository, and fingerprint are required.");
  }
  const secretVal = el("pbs-secret").value;
  const secret = secretVal ? secretVal : null;
  if (!editingPbs && !secret) return alert("A token secret is required for a new server.");
  try {
    await invoke("save_pbs_server", { server, secret });
  } catch (err) {
    return alert("save failed: " + err);
  }
  resetPbsForm();
  loadPbsServers();
}

async function loadPbsServers() {
  let servers = [];
  try {
    servers = await invoke("list_pbs_servers");
  } catch (err) {
    /* engine offline */
  }
  const list = el("pbs-list");
  list.innerHTML = "";
  if (!servers.length) {
    list.innerHTML = '<div class="placeholder">No PBS servers yet. Add one above.</div>';
    return;
  }
  for (const server of servers) {
    const row = document.createElement("div");
    row.className = "job-row";
    const main = document.createElement("div");
    main.className = "job-main";
    main.innerHTML =
      `<div class="job-name">${escapeHtml(server.name)}</div>` +
      `<div class="job-meta">${escapeHtml(server.repository)}</div>`;
    const actions = document.createElement("div");
    actions.className = "job-actions";
    actions.append(
      mkbtn("Edit", "", () => editPbsServer(server)),
      mkbtn("Delete", "", async () => {
        if (!confirm(`Delete PBS server "${server.name}"?`)) return;
        try {
          await invoke("delete_pbs_server", { id: server.id });
        } catch (err) {
          alert("delete failed: " + err);
        }
        loadPbsServers();
      }),
    );
    row.append(main, actions);
    list.append(row);
  }
}

function renderSqlInstanceCard(inst, card) {
  const badges = [`<span class="badge">${escapeHtml(sourceLabel[inst.source] || inst.source)}</span>`];
  if (inst.port) badges.push(`<span class="badge">tcp ${inst.port}</span>`);
  const auth = authModeLabel[inst.auth_mode];
  if (auth) badges.push(`<span class="badge">${escapeHtml(auth)}</span>`);
  if (inst.clustered) badges.push('<span class="badge">clustered</span>');
  if (inst.tcp_enabled === false) badges.push('<span class="badge badge-warn">TCP/IP off</span>');

  let body;
  if (inst.probe) {
    body =
      `<div class="sql-meta muted">${escapeHtml(topologyLabel(inst.probe.topology))} · ` +
      `${escapeHtml(inst.probe.edition)} · ${escapeHtml(inst.probe.product_version)}</div>` +
      `<div class="sql-dbs">${renderSqlDatabases(inst.probe.databases)}</div>`;
  } else if (inst.probe_error) {
    body = `<div class="sql-meta placeholder">unreachable: ${escapeHtml(inst.probe_error)}</div>`;
  } else if (inst.tcp_enabled === false) {
    body =
      '<div class="sql-meta placeholder">TCP/IP is disabled on this instance. Enable it in ' +
      "SQL Server Configuration Manager and restart the service before probing.</div>";
  } else {
    body = '<div class="sql-meta muted">not yet probed</div>';
  }

  const checksHtml = inst.checks
    ? `<div class="sql-checks">${inst.checks.map(renderCheck).join("")}</div>`
    : "";

  card.innerHTML =
    `<div class="sql-head"><span class="sql-server">${escapeHtml(inst.server)}</span>` +
    badges.join("") +
    '<span class="spacer"></span>' +
    `<button type="button" class="sql-save-btn">Save connection</button>` +
    `<button type="button" class="sql-check-btn">Check</button>` +
    `<button type="button" class="sql-probe-btn">${inst.probe ? "Re-probe" : "Probe"}</button>` +
    `</div><div class="sql-meta muted">instance: ${escapeHtml(inst.instance_name)}</div>` +
    body +
    checksHtml;
  card.querySelector(".sql-probe-btn").onclick = () => probeInstance(inst, card);
  card.querySelector(".sql-check-btn").onclick = () => checkInstance(inst, card);
  card.querySelector(".sql-save-btn").onclick = () => saveSqlConnection(inst);
  card.querySelectorAll(".sql-db-backup").forEach((btn) => {
    btn.onclick = () => backupDatabase(inst, btn.dataset.db);
  });
  card.querySelectorAll(".sql-db-pbs").forEach((btn) => {
    btn.onclick = () => backupDatabaseToPbs(inst, btn.dataset.db);
  });
}

function renderCheck(c) {
  const glyph = { ok: "✓", warn: "!", fail: "✗" }[c.status] || "•";
  const hint = c.hint ? `<div class="check-hint">${escapeHtml(c.hint)}</div>` : "";
  return (
    `<div class="check check-${escapeHtml(c.status)}"><span class="check-glyph">${glyph}</span>` +
    `<div class="check-body"><span class="check-name">${escapeHtml(c.name)}</span> ` +
    `<span class="muted">${escapeHtml(c.detail)}</span>${hint}</div></div>`
  );
}

// Run readiness checks (connectivity, login, sysadmin) against an instance.
async function checkInstance(inst, card) {
  const btn = card.querySelector(".sql-check-btn");
  btn.disabled = true;
  btn.textContent = "checking...";
  try {
    inst.checks = await invoke("check_sql", {
      server: inst.server,
      port: inst.port ?? null,
      auth: { kind: "integrated" },
      password: null,
    });
  } catch (err) {
    inst.checks = [{ name: "Check", status: "fail", detail: String(err), hint: null }];
  }
  renderSqlInstanceCard(inst, card);
}

// Probe an instance with the engine's service identity (integrated auth), the
// common on-host case. Credentialed connect (SQL / explicit Windows) follows.
async function probeInstance(inst, card) {
  const btn = card.querySelector(".sql-probe-btn");
  btn.disabled = true;
  btn.textContent = "probing...";
  try {
    inst.probe = await invoke("probe_sql", {
      server: inst.server,
      port: inst.port ?? null,
      auth: { kind: "integrated" },
      password: null,
    });
    inst.probe_error = null;
  } catch (err) {
    inst.probe_error = String(err);
    inst.probe = null;
  }
  renderSqlInstanceCard(inst, card);
}

function renderSqlInstances(instances) {
  const list = el("sql-list");
  list.innerHTML = "";
  if (!instances.length) {
    list.innerHTML = '<div class="placeholder">No SQL Server instances found.</div>';
    return;
  }
  for (const inst of instances) {
    const card = document.createElement("div");
    card.className = "sql-instance";
    renderSqlInstanceCard(inst, card);
    list.append(card);
  }
}

async function discoverSql() {
  const list = el("sql-list");
  const btn = el("discover-sql");
  btn.disabled = true;
  list.innerHTML = loadingHtml("discovering SQL Server instances...");
  try {
    const instances = await invoke("discover_sql", { includeNetwork: false, targets: [] });
    renderSqlInstances(instances);
  } catch (err) {
    list.innerHTML = `<div class="placeholder">error: ${escapeHtml(err)}</div>`;
  } finally {
    btn.disabled = false;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  el("tab-jobs").onclick = () => showView("jobs");
  el("tab-browse").onclick = () => {
    showView("browse");
    populateBrowseJobs();
  };
  el("tab-sql").onclick = () => {
    showView("sql");
    populatePbsServers();
    loadSqlConnections();
  };
  el("tab-pbs").onclick = () => {
    showView("pbs");
    loadPbsServers();
  };
  el("discover-sql").onclick = discoverSql;
  el("pbs-form").addEventListener("submit", savePbsServer);
  el("pbs-clear").onclick = resetPbsForm;
  el("new-job").onclick = () => openEditor(null);
  el("cancel-edit").onclick = () => showView("jobs");
  el("wiz-next").onclick = () => showWizStep(wizStep + 1);
  el("wiz-back").onclick = () => showWizStep(wizStep - 1);
  el("wiz-save").onclick = saveJob;
  el("f-source-type").onchange = updateSourceType;
  el("f-dest-type").onchange = updateDestType;
  el("load-dbs").onclick = loadDatabasesForConn;
  el("pick-folder").onclick = async () => {
    const dir = await invoke("pick_destination");
    if (dir) el("f-folder").value = dir;
  };
  el("add-folders").onclick = async () =>
    renderSources([...currentSources, ...(await invoke("pick_folders"))]);
  el("add-files").onclick = async () =>
    renderSources([...currentSources, ...(await invoke("pick_files"))]);
  el("f-schedule-kind").onchange = updateScheduleFields;
  // Auto-fill the Backup id from the name until the user edits it directly.
  el("f-name").addEventListener("input", () => {
    if (!backupIdTouched) el("f-backup-id").value = slug(el("f-name").value);
  });
  el("f-backup-id").addEventListener("input", () => {
    backupIdTouched = true;
  });
  el("load-snapshots").onclick = loadSnapshots;
  el("restore-all").onclick = () => doRestore(true);
  el("restore-selected").onclick = () => doRestore(false);
  checkEngine();
  setInterval(checkEngine, 5000);
  invoke("build_info")
    .then((v) => (el("build-info").textContent = v))
    .catch(() => {});
  loadJobs();
});
