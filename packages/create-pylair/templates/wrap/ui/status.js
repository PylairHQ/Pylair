// The local starting / restarting page. It runs only on the app's own local
// origin (the one Tauri grants IPC to), never on the wrapped page: the plugin
// navigates the window to the server's url once it is ready, and back here
// whenever the server is gone. The plugin replays the last state and the
// server's log to a page that loads late, so this page can start empty.
(function () {
  var tauri = window.__TAURI__;
  var page = document.body.dataset.page; // "starting" | "restarting"
  var title = document.getElementById("title");
  var reason = document.getElementById("reason");
  var log = document.getElementById("log");
  var retry = document.getElementById("retry");
  var lines = [];

  function render(state, detail) {
    if (state === "failed") {
      title.textContent = "The app keeps stopping";
      reason.textContent = detail || "It crashed several times in a row.";
      retry.hidden = false;
    } else if (state === "restarting" || page === "restarting") {
      title.textContent = "Restarting the app";
      reason.textContent = detail ? "Why: " + detail : "";
      retry.hidden = true;
    } else {
      title.textContent = "Starting the app";
      reason.textContent = detail || "";
      retry.hidden = true;
    }
  }

  function addLine(line) {
    lines.push(String(line).replace(/\r$/, ""));
    if (lines.length > 200) lines.shift();
    log.textContent = lines.join("\n");
    log.scrollTop = log.scrollHeight;
  }

  render(page === "restarting" ? "restarting" : "starting", "");
  if (!tauri) {
    reason.textContent = "This page needs the app window.";
    return;
  }
  tauri.event.listen("pylair://state", function (e) {
    var p = e.payload || {};
    render(p.to, p.detail);
  });
  tauri.event.listen("pylair://stderr", function (e) {
    addLine(e.payload);
  });
  retry.addEventListener("click", function () {
    tauri.core.invoke("plugin:pylair|python_retry").catch(function () {});
  });
  // Subscribes this window, and shows the state if nothing is replayed.
  tauri.core.invoke("plugin:pylair|python_state").then(function (s) {
    if (s && s.state && !lines.length) render(s.state, "");
  }).catch(function () {});
})();
