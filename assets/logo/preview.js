"use strict";

const {studies, fonts} = window.MONIKA_DATA;
const {toSRGB, css: colorCSS} = window.MonikaColor;
const INITIAL = {ink: [31.59, .0045, 248], accent: [85.4, .2441, 147.4]};
const channels = [
  {letter: "L", name: "明るさ %", min: 0, max: 100, step: .01},
  {letter: "C", name: "鮮やかさ", min: 0, max: .4, step: .0001},
  {letter: "h", name: "色相 °", min: 0, max: 360, step: .1},
];
const state = {ink: [...INITIAL.ink], accent: [...INITIAL.accent], mode: "accent", font: "mono"};
const params = new URLSearchParams(location.hash.slice(1));
for (const key of ["ink", "accent"]) {
  if (params.has(key)) {
    const values = params.get(key).split(",").map(Number);
    if (values.length !== 3 || values.some((n, i) => !Number.isFinite(n) || n<channels[i].min || n>channels[i].max)) {
      throw new Error(`URL の ${key} に有効な OKLCH の値を指定してください。`);
    }
    state[key] = values;
  }
}

const controls = document.querySelector("#color-controls");
for (const [key, label] of [["ink", "基調色"], ["accent", "アクセント"]]) {
  const section = document.createElement("section");
  section.className = "color-control";
  section.dataset.color = key;
  section.setAttribute("aria-label", `${label}の OKLCH`);
  section.innerHTML = `<div class="color-title"><span class="swatch" aria-hidden="true"></span><h3>${label}</h3><span class="hex"></span></div>`;
  channels.forEach((channel, index) => {
    const id = `${key}-${channel.letter}`;
    section.insertAdjacentHTML("beforeend", `<div class="channel">
      <label for="${id}-number"><b>${channel.letter}</b>${channel.name}</label>
      <input id="${id}-number" type="number" min="${channel.min}" max="${channel.max}" step="any" required data-key="${key}" data-channel="${index}" aria-label="${label} ${channel.name} 数値">
      <input id="${id}" type="range" min="${channel.min}" max="${channel.max}" step="${channel.step}" data-key="${key}" data-channel="${index}" aria-label="${label} ${channel.name}">
    </div>`);
  });
  section.insertAdjacentHTML("beforeend", `<input class="color-code" readonly aria-label="${label}の OKLCH 表記"><p class="gamut-note"></p>`);
  controls.append(section);
}

function updateColors(persist = true) {
  const colors = {ink: toSRGB(state.ink), accent: toSRGB(state.accent)};
  document.documentElement.style.setProperty("--logo-ink", colors.ink.hex);
  document.documentElement.style.setProperty("--logo-accent", state.mode === "mono" ? colors.ink.hex : colors.accent.hex);
  for (const key of ["ink", "accent"]) {
    const section = controls.querySelector(`[data-color=${key}]`);
    section.querySelector(".swatch").style.background = colors[key].hex;
    section.querySelector(".hex").textContent = colors[key].hex;
    section.querySelector(".color-code").value = colorCSS(state[key]);
    section.querySelector(".gamut-note").textContent = colors[key].mapped ? `sRGB の範囲に C を ${colors[key].mappedC.toFixed(4)} へ調整` : "指定した色は sRGB の色域内です。";
    section.querySelectorAll("[data-channel]").forEach(input => {
      const index = Number(input.dataset.channel);
      if (input !== document.activeElement) input.value = state[key][index];
      if (input.type === "range") {
        const {min, max} = channels[index];
        const stops = Array.from({length: 17}, (_, i) => {
          const sample = [...state[key]];
          sample[index] = min+(max-min)*i/16;
          return `${toSRGB(sample).hex} ${i/16*100}%`;
        });
        input.style.setProperty("--track", `linear-gradient(to right, ${stops.join(",")})`);
      }
    });
  }
  if (persist) {
    const saved = new URLSearchParams({ink: state.ink.join(","), accent: state.accent.join(",")});
    history.replaceState(null, "", `#${saved}`);
  }
}

controls.addEventListener("input", event => {
  const input = event.target;
  if (!input.matches("[data-channel]") || input.value === "" || !input.validity.valid) return;
  state[input.dataset.key][Number(input.dataset.channel)] = input.valueAsNumber;
  updateColors();
});
controls.addEventListener("change", event => {
  if (event.target.matches("input[type=number]") && !event.target.validity.valid) event.target.reportValidity();
});
controls.querySelectorAll(".color-code").forEach(input => input.addEventListener("focus", () => input.select()));
document.querySelector("#reset-colors").addEventListener("click", () => {
  state.ink = [...INITIAL.ink];
  state.accent = [...INITIAL.accent];
  updateColors();
});

function makeSVG(study, kind, exported = false) {
  const font = fonts.find(item => item.id === state.font);
  const width = kind === "mark" ? 96 : font.width;
  const height = kind === "mark" ? 96 : 112;
  let body = kind === "mark" ? study.mark : `<g transform="translate(8 8)">${study.mark}</g>${font.lettering}`;
  const ink = exported ? toSRGB(state.ink).hex : "var(--logo-ink)";
  const accent = exported ? (state.mode === "mono" ? ink : toSRGB(state.accent).hex) : "var(--logo-accent)";
  body = body.replaceAll("#303234", ink).replaceAll("#22F765", accent);
  const title = `Monika ${study.label} ${study.name}${kind === "mark" ? " マーク" : ""}`;
  const metadata = exported ? `<metadata>${JSON.stringify({ink: {oklch: colorCSS(state.ink), srgb: ink}, accent: {oklch: colorCSS(state.accent), srgb: toSRGB(state.accent).hex}, mode: state.mode, font: font.label})}</metadata>` : "";
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img" aria-label="${title}"><title>${title}</title>${metadata}${body}</svg>`;
}

function buildCards() {
  const grid = document.querySelector("#studies");
  grid.replaceChildren();
  studies.forEach(study => {
    const article = document.createElement("article");
    article.className = "study";
    article.dataset.study = study.id;
    article.innerHTML = `<div class="study-head"><span class="study-code">${study.label}</span><h3>${study.name}</h3>${study.label === "C1" ? '<span class="tag">原案</span>' : ''}</div>
      <div class="stage"><div class="symbol">${makeSVG(study, "mark")}</div><div class="lockup">${makeSVG(study, "logo")}</div></div>
      <p class="study-description">${study.description}</p>
      <div class="small-sizes">${[16,24,32].map(size => `<div class="small-size">${makeSVG(study, "mark").replace('width="96" height="96"', `width="${size}" height="${size}"`)}<span>${size}</span></div>`).join("")}</div>
      <div class="downloads"><button type="button" data-export="logo-svg">ロゴ SVG</button><button type="button" data-export="logo-png">PNG</button><button type="button" data-export="mark-svg">マーク SVG</button><button type="button" data-export="mark-png">PNG</button></div>`;
    article.querySelectorAll("[data-export]").forEach(button => {
      const [kind, format] = button.dataset.export.split("-");
      button.setAttribute("aria-label", `${study.label} ${kind === "mark" ? "マーク" : "ロゴ"} ${format.toUpperCase()} を保存`);
      button.addEventListener("click", () => exportAsset(study, kind, format, button));
    });
    grid.append(article);
  });
}

document.querySelectorAll("[data-mode]").forEach(button => button.addEventListener("click", () => {
  state.mode = button.dataset.mode;
  document.querySelectorAll("[data-mode]").forEach(item => item.setAttribute("aria-pressed", String(item === button)));
  updateColors(false);
}));
document.querySelectorAll("button[data-background]").forEach(button => button.addEventListener("click", () => {
  document.body.dataset.background = button.dataset.background;
  document.querySelectorAll("button[data-background]").forEach(item => item.setAttribute("aria-pressed", String(item === button)));
}));
document.querySelector("#font").addEventListener("change", event => {
  state.font = event.target.value;
  document.querySelectorAll(".study").forEach((article, i) => article.querySelector(".lockup").innerHTML = makeSVG(studies[i], "logo"));
});

function download(blob, filename) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}

async function exportAsset(study, kind, format, button) {
  const status = document.querySelector("#export-status");
  button.disabled = true;
  // Capture content and filename before asynchronous PNG rendering.
  const svg = makeSVG(study, kind, true);
  const name = `monika-${study.label.toLowerCase()}-${kind}-${state.font}-${state.mode}`;
  const width = kind === "mark" ? 96 : fonts.find(item => item.id === state.font).width;
  const height = kind === "mark" ? 96 : 112;
  try {
    const blob = new Blob([svg], {type: "image/svg+xml;charset=utf-8"});
    if (format === "svg") download(blob, `${name}.svg`);
    else {
      const url = URL.createObjectURL(blob);
      try {
        const image = new Image();
        image.src = url;
        await image.decode();
        const canvas = document.createElement("canvas");
        canvas.width = Math.ceil(width*4);
        canvas.height = Math.ceil(height*4);
        const context = canvas.getContext("2d", {colorSpace: "srgb"});
        context.drawImage(image, 0, 0, canvas.width, canvas.height);
        const png = await new Promise(resolve => canvas.toBlob(resolve, "image/png"));
        if (!png) throw new Error("PNG を作成できませんでした。");
        download(png, `${name}.png`);
      } finally { URL.revokeObjectURL(url); }
    }
    status.textContent = `${study.label} の${kind === "mark" ? "マーク" : "ロゴ"}を、現在の配色で保存しました。`;
  } catch (error) {
    status.textContent = `書き出しに失敗しました：${error.message}`;
    console.error(error);
  } finally { button.disabled = false; }
}

buildCards();
updateColors(false);
