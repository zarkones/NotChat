// Decode a QR from a user-picked image file (desktop / no-camera fallback).
// Expects <input type="file" id="oc-qr-file"> already chosen (files[0]).
if (typeof jsQR !== "function") {
  dioxus.send("ERR:jsQR missing");
  return "missing-jsqr";
}
const input = document.getElementById("oc-qr-file");
if (!input || !input.files || !input.files[0]) {
  dioxus.send("ERR:no file selected");
  return "nofile";
}
const file = input.files[0];
try {
  const bmp = await createImageBitmap(file);
  const canvas = document.getElementById("oc-qr-canvas") || document.createElement("canvas");
  canvas.width = bmp.width;
  canvas.height = bmp.height;
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.drawImage(bmp, 0, 0);
  const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
  const code = jsQR(img.data, img.width, img.height, { inversionAttempts: "attemptBoth" });
  if (code && code.data) {
    dioxus.send(String(code.data));
    return "ok";
  }
  dioxus.send("ERR:no QR in image");
  return "none";
} catch (e) {
  dioxus.send("ERR:" + ((e && e.message) ? e.message : String(e)));
  return "error";
}
