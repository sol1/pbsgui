// pbsgui frontend.
//
// Uses the global Tauri API (app.withGlobalTauri = true) so there is no JS build
// step yet. A bundler and framework can be introduced later without changing the
// Rust side.

const invoke = window.__TAURI__?.core?.invoke;

function switchView(name) {
  document.querySelectorAll(".nav-item").forEach((b) => {
    b.classList.toggle("active", b.dataset.view === name);
  });
  document.querySelectorAll(".view").forEach((v) => {
    v.classList.toggle("active", v.id === `view-${name}`);
  });
}

function wireNav() {
  document.querySelectorAll(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => switchView(btn.dataset.view));
  });
}

async function refreshEngineStatus() {
  const el = document.getElementById("engine-status");
  if (!invoke) {
    el.textContent = "unavailable (open in the app)";
    return;
  }
  try {
    el.textContent = await invoke("engine_status");
  } catch (err) {
    el.textContent = `error: ${err}`;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  wireNav();
  refreshEngineStatus();
});
