"use strict";

// Oklab matrices: Björn Ottosson's public-domain reference implementation.
// https://bottosson.github.io/posts/oklab/#converting-from-linear-srgb-to-oklab
// Preserve L/h and reduce C to the sRGB boundary when needed.
window.MonikaColor = (() => {
  function linearRGB([lightness, chroma, hue]) {
    const L = lightness / 100, angle = hue * Math.PI / 180;
    const a = chroma * Math.cos(angle), b = chroma * Math.sin(angle);
    const l = (L + .3963377774*a + .2158037573*b) ** 3;
    const m = (L - .1055613458*a - .0638541728*b) ** 3;
    const s = (L - .0894841775*a - 1.2914855480*b) ** 3;
    return [4.0767416621*l - 3.3077115913*m + .2309699292*s,
      -1.2684380046*l + 2.6097574011*m - .3413193965*s,
      -.0041960863*l - .7034186147*m + 1.7076147010*s];
  }
  function inGamut(rgb) { return rgb.every(v => v >= -1e-8 && v <= 1+1e-8); }
  function encode(v) { return v <= .0031308 ? 12.92*v : 1.055*v**(1/2.4)-.055; }
  function toSRGB(color) {
    const [L, C, h] = color;
    let mappedC = C;
    if (L === 0 || L === 100) mappedC = 0;
    else if (!inGamut(linearRGB(color))) {
      let lo = 0, hi = C;
      for (let i=0; i<28; i++) {
        const mid = (lo+hi)/2;
        if (inGamut(linearRGB([L, mid, h]))) lo = mid;
        else hi = mid;
      }
      mappedC = lo;
    }
    const rgb = linearRGB([L, mappedC, h]).map(v => Math.round(Math.max(0, Math.min(1, encode(v)))*255));
    const hex = "#"+rgb.map(v => v.toString(16).padStart(2, "0")).join("").toUpperCase();
    return {hex, rgb, mappedC, mapped: C-mappedC > .00001};
  }
  function css(color) { return `oklch(${Number(color[0].toFixed(4))}% ${Number(color[1].toFixed(6))} ${Number(color[2].toFixed(4))})`; }
  return {linearRGB, toSRGB, css};
})();
