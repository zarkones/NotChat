// NotChat QR scanner body (runs inside dioxus document::eval).
// Expects window.jsQR (vendored) and optional native BarcodeDetector.
// Sends raw string via dioxus.send on success, or "ERR:…" on failure/cancel.
// Cancel: set window.__ocQrStop = true (from Rust via a second eval).

window.__ocQrStop = false;

const video = document.getElementById("oc-qr-video");
const canvas = document.getElementById("oc-qr-canvas");
if (!video || !canvas) {
  dioxus.send("ERR:scanner UI missing");
  return "missing";
}

let stream = null;

const stopAll = () => {
  window.__ocQrStop = true;
  try {
    if (stream) {
      stream.getTracks().forEach((t) => t.stop());
      stream = null;
    }
  } catch (_) {}
  try {
    video.srcObject = null;
    video.style.display = "none";
  } catch (_) {}
};

try {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    dioxus.send("ERR:camera API unavailable — paste invite or load QR image");
    return "nocam";
  }

  video.style.display = "block";
  stream = await navigator.mediaDevices.getUserMedia({
    video: { facingMode: { ideal: "environment" } },
    audio: false,
  });
  video.srcObject = stream;
  await video.play();

  let detector = null;
  if ("BarcodeDetector" in window) {
    try {
      detector = new BarcodeDetector({ formats: ["qr_code"] });
    } catch (_) {
      detector = null;
    }
  }

  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  const deadline = Date.now() + 45000;

  while (!window.__ocQrStop && Date.now() < deadline) {
    let raw = null;

    if (detector) {
      try {
        const codes = await detector.detect(video);
        if (codes && codes.length > 0 && codes[0].rawValue) {
          raw = codes[0].rawValue;
        }
      } catch (_) {}
    }

    if (!raw && typeof jsQR === "function") {
      try {
        const w = video.videoWidth || 0;
        const h = video.videoHeight || 0;
        if (w > 0 && h > 0) {
          canvas.width = w;
          canvas.height = h;
          ctx.drawImage(video, 0, 0, w, h);
          const img = ctx.getImageData(0, 0, w, h);
          const code = jsQR(img.data, img.width, img.height, {
            inversionAttempts: "dontInvert",
          });
          if (code && code.data) {
            raw = code.data;
          }
        }
      } catch (_) {}
    }

    if (raw) {
      stopAll();
      dioxus.send(String(raw));
      return "ok";
    }

    await new Promise((r) => setTimeout(r, 120));
  }

  const wasStop = !!window.__ocQrStop;
  stopAll();
  if (wasStop) {
    dioxus.send("ERR:cancelled");
  } else {
    dioxus.send("ERR:timeout — no QR found");
  }
  return "done";
} catch (e) {
  stopAll();
  const msg = (e && e.message) ? e.message : String(e);
  dioxus.send("ERR:" + msg);
  return "error";
}
