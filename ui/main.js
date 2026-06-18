// pbsgui frontend.
//
// Uses the global Tauri API (app.withGlobalTauri = true), so there is no JS build
// step yet. Talks to two commands: engine_ping and run_backup. run_backup streams
// engine replies over a Tauri channel.

const { invoke, Channel } = window.__TAURI__.core;

const el = (id) => document.getElementById(id);

function setEngineStatus(text) {
  el("engine-status").textContent = text;
}

function appendLog(line) {
  const log = el("log");
  log.textContent += line + "\n";
  log.scrollTop = log.scrollHeight;
}

function setProgress(fraction, label) {
  el("progress-bar").style.width = `${Math.round(fraction * 100)}%`;
  el("progress-label").textContent = label;
}

async function checkEngine() {
  setEngineStatus("connecting...");
  try {
    await invoke("engine_ping");
    setEngineStatus("connected");
  } catch (err) {
    setEngineStatus(`unavailable (${err})`);
  }
}

function setBusy(busy) {
  el("backup-btn").disabled = busy;
  el("test-btn").disabled = busy;
}

async function runBackup(event) {
  event.preventDefault();
  setBusy(true);
  el("log").textContent = "";
  setProgress(0, "starting...");

  const config = {
    repository: el("repository").value.trim(),
    secret: el("secret").value,
    fingerprint: el("fingerprint").value.trim(),
    backup_id: el("backup_id").value.trim(),
    path: el("path").value,
  };

  const channel = new Channel();
  channel.onmessage = (reply) => {
    switch (reply.reply) {
      case "accepted":
        appendLog(`accepted: ${reply.job_id}`);
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
        break;
      case "error":
        appendLog("error: " + reply.message);
        break;
    }
  };

  try {
    await invoke("run_backup", { config, on_event: channel });
  } catch (err) {
    appendLog("error: " + err);
    setProgress(0, "failed");
  } finally {
    setBusy(false);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  el("backup-form").addEventListener("submit", runBackup);
  el("test-btn").addEventListener("click", checkEngine);
  checkEngine();
});
