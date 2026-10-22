// Draws whatever screen the app sends and reports what the user did. Setup
// decisions all happen in Rust; this file only fills in and shows sections.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const send = (event) => invoke("send", { event });
const qrWaiting = document.querySelector('[data-field="qr"] .waiting').cloneNode(true);

function show(name) {
  for (const section of document.querySelectorAll("[data-screen]")) {
    section.hidden = section.dataset.screen !== name;
  }
  const section = document.querySelector(`[data-screen="${name}"]`);
  const field = section && section.querySelector("input:not([type=radio])");
  if (field) field.focus();
}

function fill(section, name, text) {
  for (const el of section.querySelectorAll(`[data-field="${name}"]`)) {
    el.textContent = text;
  }
}

function render(view) {
  const section = document.querySelector(`[data-screen="${view.screen}"]`);
  if (!section) return;
  for (const button of section.querySelectorAll("button")) button.disabled = false;

  switch (view.screen) {
    case "linking": {
      fill(section, "device_name", view.device_name);
      const box = section.querySelector('[data-field="qr"]');
      // The SVG comes from the qrcode crate in Rust, never from user input.
      if (view.qr) box.innerHTML = view.qr;
      else box.replaceChildren(qrWaiting.cloneNode(true));
      break;
    }
    case "requesting_code":
    case "enter_code":
      fill(section, "number", view.number);
      break;
    case "importing_history": {
      const bar = section.querySelector('[data-field="percent"]');
      if (view.percent === null) bar.removeAttribute("value");
      else bar.value = view.percent;
      fill(section, "imported", view.imported.toLocaleString());
      fill(section, "note", view.note);
      break;
    }
    case "history_unavailable":
      for (const p of section.querySelectorAll("[data-reason]")) {
        p.hidden = p.dataset.reason !== view.reason;
      }
      break;
    case "importing_desktop":
      fill(section, "imported", view.imported.toLocaleString());
      break;
    case "done": {
      const list = section.querySelector('[data-field="clients"]');
      list.replaceChildren(...view.clients.map((client) => {
        const item = document.createElement("li");
        item.className = client.problem ? "bad" : "ok";
        item.textContent = client.problem ? `${client.name} isn't connected` : `${client.name} is connected`;
        if (client.problem) {
          const why = document.createElement("small");
          why.textContent = client.problem;
          item.append(why);
        }
        return item;
      }));
      section.querySelector('[data-field="restart"]').hidden = !view.can_restart_desktop;
      section.querySelector('[data-field="no_restart"]').hidden = view.can_restart_desktop;
      break;
    }
    case "failed":
      fill(section, "message", view.message);
      section.querySelector('[data-field="skip_history"]').hidden = !view.can_skip_history;
      break;
  }

  const visible = document.querySelector("[data-screen]:not([hidden])");
  if (!visible || visible.dataset.screen !== view.screen) show(view.screen);
}

function checked(name) {
  return document.querySelector(`input[name="${name}"]:checked`).value;
}

const actions = {
  choose_account: () => send({ event: "account_chosen", choice: checked("account") }),
  choose_access: () => send({ event: "access_chosen", access: checked("access") }),
  restart_desktop: () => send({ event: "restart_desktop" }),
  retry: () => send({ event: "retry" }),
  skip_history: () => send({ event: "skip_history" }),
  desktop_yes: () => send({ event: "desktop_import_chosen", import: true }),
  desktop_no: () => send({ event: "desktop_import_chosen", import: false }),
  open_captcha: async (button) => {
    const error = document.querySelector('[data-field="captcha_error"]');
    error.hidden = true;
    try {
      await invoke("open_captcha");
    } catch (message) {
      error.textContent = `The check couldn't open: ${message}. Use your web browser instead, below.`;
      error.hidden = false;
    }
    button.disabled = false;
  },
  move: async (button) => {
    const error = document.querySelector('[data-field="move_error"]');
    error.hidden = true;
    try {
      await invoke("move_to_applications");
    } catch (message) {
      error.textContent = `The app couldn't move itself: ${message}. Drag it into Applications in Finder, then open it from there.`;
      error.hidden = false;
      button.disabled = false;
    }
  },
};

const forms = {
  enter_number: (data) => send({ event: "number_entered", number: data.get("number") }),
  solve_captcha: (data) => send({ event: "captcha_solved", token: data.get("token") }),
  enter_code: (data) => send({ event: "code_entered", code: data.get("code") }),
};

document.addEventListener("click", (e) => {
  const button = e.target.closest("button[data-action]");
  if (!button) return;
  button.disabled = true;
  actions[button.dataset.action](button);
});

document.addEventListener("submit", (e) => {
  e.preventDefault();
  const form = e.target;
  form.querySelector("button").disabled = true;
  forms[form.dataset.action](new FormData(form));
  form.reset();
});

async function start() {
  if (await invoke("needs_move")) {
    show("move");
    return;
  }
  await listen("setup", (e) => render(e.payload));
  render(await invoke("current"));
}

start();
