"use strict";

const jobsEl = document.getElementById("jobs");
const template = document.getElementById("job-template");
const picker = document.getElementById("picker");

const PLATFORM_LABEL = { ps2: "PS2", x360: "360" };
const STAGE_LABEL = {
  uploading: "Uploading",
  queued: "Waiting in queue",
  detecting: "Detecting platform",
  extracting: "Extracting",
  compressing: "Compressing",
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
};
const FINISHED = new Set(["done", "failed", "cancelled"]);

function formatBytes(n) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
}

function selectedPlatform() {
  return document.querySelector('input[name="platform"]:checked').value;
}

/** Creates a job card and returns helpers to drive it. */
function createCard(name) {
  const el = template.content.firstElementChild.cloneNode(true);
  el.querySelector(".name").textContent = name;
  el.querySelector(".name").title = name;
  jobsEl.prepend(el);

  const card = {
    el,
    id: null,
    events: null,
    setPlatform(platform) {
      const badge = el.querySelector(".badge");
      badge.className = `badge ${platform ?? ""}`;
      badge.textContent = PLATFORM_LABEL[platform] ?? "ISO";
    },
    setProgress(percent, status) {
      el.querySelector(".fill").style.width = `${Math.max(0, Math.min(100, percent))}%`;
      el.querySelector(".status").textContent = status;
    },
    render(view) {
      card.setPlatform(view.platform);
      el.classList.toggle("failed", view.stage === "failed");
      let status = STAGE_LABEL[view.stage] ?? view.stage;
      if (view.stage === "done") {
        status += view.output_size != null ? ` · ${formatBytes(view.output_size)}` : "";
        const link = el.querySelector(".download");
        link.href = `api/jobs/${view.id}/download`;
        link.hidden = false;
      } else if (view.stage === "failed") {
        status = view.message ?? status;
      } else if (!FINISHED.has(view.stage)) {
        if (view.stage !== "queued") status += ` ${view.percent.toFixed(1)}%`;
        if (view.message) status += ` · ${view.message}`;
      }
      card.setProgress(view.stage === "done" ? 100 : view.percent, status);
      if (FINISHED.has(view.stage)) card.events?.close();
    },
    follow(id) {
      card.id = id;
      card.events = new EventSource(`api/jobs/${id}/events`);
      card.events.onmessage = (e) => card.render(JSON.parse(e.data));
    },
  };
  card.setPlatform(null);
  return card;
}

function upload(file) {
  const card = createCard(file.name);
  if (!file.name.toLowerCase().endsWith(".iso")) {
    card.render({ stage: "failed", message: "Only .iso files are supported", percent: 0 });
    card.el.querySelector(".remove").onclick = () => card.el.remove();
    return;
  }

  const xhr = new XMLHttpRequest();
  xhr.open("PUT", "api/jobs");
  xhr.setRequestHeader("X-Filename", encodeURIComponent(file.name));
  xhr.setRequestHeader("X-Platform", selectedPlatform());
  xhr.setRequestHeader("Content-Type", "application/octet-stream");

  xhr.upload.onprogress = (e) => {
    if (!e.lengthComputable) return;
    const pct = (e.loaded / e.total) * 100;
    card.setProgress(pct, `Uploading ${pct.toFixed(1)}% · ${formatBytes(e.loaded)} / ${formatBytes(e.total)}`);
  };
  xhr.onload = () => {
    if (xhr.status === 200) {
      const view = JSON.parse(xhr.responseText);
      card.render(view);
      card.follow(view.id);
    } else {
      card.render({ stage: "failed", message: xhr.responseText || `HTTP ${xhr.status}`, percent: 0 });
    }
  };
  xhr.onerror = () => card.render({ stage: "failed", message: "Upload failed (server unreachable?)", percent: 0 });

  card.el.querySelector(".remove").onclick = () => {
    if (card.id) {
      card.events?.close();
      fetch(`api/jobs/${card.id}`, { method: "DELETE" });
    } else {
      xhr.abort();
    }
    card.el.remove();
  };

  xhr.send(file);
}

function handleFiles(files) {
  for (const file of files) upload(file);
}

// Jobs survive a page reload as long as the server is running.
async function restoreJobs() {
  const res = await fetch("api/jobs");
  if (!res.ok) return;
  for (const view of await res.json()) {
    const card = createCard(view.name);
    card.id = view.id;
    card.render(view);
    if (!FINISHED.has(view.stage)) card.follow(view.id);
    card.el.querySelector(".remove").onclick = () => {
      card.events?.close();
      fetch(`api/jobs/${view.id}`, { method: "DELETE" });
      card.el.remove();
    };
  }
}

picker.addEventListener("change", () => {
  handleFiles(picker.files);
  picker.value = "";
});

let dragDepth = 0;
window.addEventListener("dragenter", (e) => {
  e.preventDefault();
  dragDepth++;
  document.body.classList.add("dragging");
});
window.addEventListener("dragleave", () => {
  if (--dragDepth === 0) document.body.classList.remove("dragging");
});
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("drop", (e) => {
  e.preventDefault();
  dragDepth = 0;
  document.body.classList.remove("dragging");
  handleFiles(e.dataTransfer.files);
});

restoreJobs();
