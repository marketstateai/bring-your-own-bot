const invoke = (command, args = {}) => window.__TAURI__.core.invoke(command, args);

const elements = {
  overall: document.querySelector("#overall-status"),
  serviceDot: document.querySelector("#service-dot"),
  serviceDetail: document.querySelector("#service-detail"),
  codexDot: document.querySelector("#codex-dot"),
  codexDetail: document.querySelector("#codex-detail"),
  accountDot: document.querySelector("#account-dot"),
  accountDetail: document.querySelector("#account-detail"),
  ownerDot: document.querySelector("#owner-dot"),
  ownerDetail: document.querySelector("#owner-detail"),
  autostart: document.querySelector("#autostart"),
  message: document.querySelector("#message"),
  connect: document.querySelector("#connect-button"),
  test: document.querySelector("#test-button"),
  restart: document.querySelector("#restart-button"),
  install: document.querySelector("#install-button"),
  unlink: document.querySelector("#unlink-button"),
  quit: document.querySelector("#quit-button"),
  endpoint: document.querySelector("#endpoint")
};

let status = null;
let changingAutostart = false;

function dot(element, tone) {
  element.className = `status-dot ${tone}`;
}

function setMessage(message = "", error = false) {
  elements.message.textContent = message;
  elements.message.classList.toggle("error", error);
}

function render(next) {
  status = next;
  elements.endpoint.textContent = next.endpoint.replace("http://", "");
  elements.autostart.checked = next.launchAtLogin;

  dot(elements.serviceDot, next.serviceRunning ? "good" : "bad");
  elements.serviceDetail.textContent = next.serviceRunning
    ? next.externallyManaged
      ? "Running through an existing local bridge"
      : `Running locally on ${next.endpoint}`
    : next.lastError || "Connector is stopped";

  dot(elements.codexDot, next.codexFound ? "good" : "bad");
  elements.codexDetail.textContent = next.codexFound
    ? `${next.codexVersion || "Codex installed"} · bundled with this connector`
    : "Codex is not available";

  dot(elements.accountDot, next.authenticated ? "good" : next.loginInProgress ? "warn" : "bad");
  elements.accountDetail.textContent = next.loginInProgress
    ? "Waiting for OpenAI sign-in to finish"
    : next.authStatus;

  dot(elements.ownerDot, next.ownerBound ? "good" : "warn");
  elements.ownerDetail.textContent = next.ownerBound
    ? `Linked to ${next.ownerEmail || "this MarketState account"}`
    : "Open Orama while signed in to link this connector";

  const ready = next.serviceRunning && next.codexFound && next.authenticated;
  elements.overall.textContent = ready ? "Ready" : next.loginInProgress ? "Connecting" : "Action needed";
  elements.overall.className = `status-chip ${ready ? "ready" : "attention"}`;

  elements.connect.hidden = next.authenticated || !next.codexFound;
  elements.connect.disabled = next.loginInProgress;
  elements.connect.textContent = next.loginInProgress ? "Connecting…" : "Connect Codex";
  elements.install.hidden = next.codexFound;
  elements.unlink.hidden = !next.ownerBound;
  elements.test.disabled = !ready;
  elements.restart.disabled = next.externallyManaged;
}

async function refresh() {
  try {
    render(await invoke("connector_status"));
  } catch (error) {
    setMessage(String(error), true);
  }
}

elements.autostart.addEventListener("change", async () => {
  if (changingAutostart) return;
  changingAutostart = true;
  try {
    const enabled = await invoke("set_launch_at_login", { enabled: elements.autostart.checked });
    elements.autostart.checked = enabled;
    setMessage(enabled ? "The connector will start automatically." : "Automatic startup is off.");
  } catch (error) {
    elements.autostart.checked = status?.launchAtLogin ?? false;
    setMessage(String(error), true);
  } finally {
    changingAutostart = false;
  }
});

elements.connect.addEventListener("click", async () => {
  setMessage("Opening OpenAI sign-in…");
  try {
    await invoke("begin_codex_login");
    await refresh();
  } catch (error) {
    setMessage(String(error), true);
  }
});

elements.test.addEventListener("click", async () => {
  elements.test.disabled = true;
  setMessage("Asking Codex for a short confirmation…");
  try {
    const reply = await invoke("test_connection");
    setMessage(`Codex replied: ${reply}`);
  } catch (error) {
    setMessage(String(error), true);
  } finally {
    await refresh();
  }
});

elements.restart.addEventListener("click", async () => {
  setMessage("Restarting the local connector…");
  try {
    await invoke("restart_connector");
    setMessage("Connector restarted.");
  } catch (error) {
    setMessage(String(error), true);
  } finally {
    await refresh();
  }
});

elements.install.addEventListener("click", async () => {
  try {
    await invoke("open_codex_install");
  } catch (error) {
    setMessage(String(error), true);
  }
});

elements.unlink.addEventListener("click", async () => {
  if (!window.confirm("Unlink this MarketState user and sign out of Codex? The next user must connect their own Codex account.")) return;
  try {
    await invoke("unlink_marketstate_user");
    setMessage("MarketState user unlinked. Open Orama as the intended user to link again.");
    await refresh();
  } catch (error) {
    setMessage(String(error), true);
  }
});

elements.quit.addEventListener("click", () => invoke("quit_connector"));

await refresh();
window.setInterval(refresh, 3000);
