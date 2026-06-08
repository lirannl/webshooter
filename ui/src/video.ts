// ── Server-to-client datagram protocol ───────────────────────────────────────

import { ClientMessageDiscriminant, toBytes } from "./ClientMessage";
import { parseMessage, ServerMessageDiscriminant } from "./serverMessage";

export interface VideoFrameMsg {
  readonly type: typeof ServerMessageDiscriminant.VideoFrame;
  frameId: number; // u16, wrapping
  fragIdx: number;
  numFrags: number;
  isKeyframe: boolean;
  payload: Uint8Array;
}

// ── Circular u16 comparison ───────────────────────────────────────────────────
// Returns true if `a` is at or before `b` in wrapping u16 space.
// Used to decide which pending entries to evict when a frame completes.
function u16Leq(a: number, b: number): boolean {
  return ((b - a) & 0xffff) < 0x8000;
}

// ── Rendering ─────────────────────────────────────────────────────────────────

interface PendingFrame {
  fragments: (Uint8Array | null)[];
  isKeyframe: boolean;
  received: number;
}

export const render_video = async (wt: WebTransport) => {
  const canvas = document.createElement("canvas");
  canvas.style.cssText =
    "position:fixed;inset:0;width:100%;height:100%;background:#000;cursor:pointer;";
  document.body.appendChild(canvas);

  await new Promise((resolve) => setTimeout(resolve, 5));
  await wt.send({
    discriminant: ClientMessageDiscriminant.Resize,
    index: 0,
    width: canvas.offsetWidth,
    height: canvas.offsetHeight,
  });
  const ctx = canvas.getContext("2d")!;

  // Show a tap-to-resize button for 2 s whenever the canvas changes size
  // (resize observer) or enters/exits fullscreen.
  let resizeOverlay: HTMLButtonElement | null = null;
  let resizeTimer: ReturnType<typeof setTimeout> | null = null;

  const showResizePrompt = () => {
    resizeOverlay?.remove();
    if (resizeTimer !== null) clearTimeout(resizeTimer);

    const btn = document.createElement("button");
    btn.textContent = "⤢ Resize";
    btn.style.cssText = [
      "position:fixed",
      "top:50%",
      "left:50%",
      "transform:translate(-50%,-50%)",
      "padding:.6em 1.2em",
      "font-size:1rem",
      "background:rgba(0,0,0,.6)",
      "color:#fff",
      "border:2px solid rgba(255,255,255,.6)",
      "border-radius:8px",
      "cursor:pointer",
      "z-index:9999",
    ].join(";");
    document.body.appendChild(btn);
    resizeOverlay = btn;

    const dismiss = () => {
      if (resizeTimer !== null) clearTimeout(resizeTimer);
      btn.remove();
      resizeOverlay = null;
    };

    btn.addEventListener(
      "pointerdown",
      () => {
        dismiss();
        wt.datagramWriter?.write(
          toBytes({
            discriminant: ClientMessageDiscriminant.Resize,
            index: 0,
            width: canvas.offsetWidth,
            height: canvas.offsetHeight,
          }),
        );
      },
      { once: true },
    );

    resizeTimer = setTimeout(dismiss, 2000);
  };

  const ro = new ResizeObserver(showResizePrompt);
  ro.observe(canvas);
  document.addEventListener("fullscreenchange", showResizePrompt);

  const decoder = new VideoDecoder({
    output(frame) {
      if (
        canvas.width !== frame.displayWidth ||
        canvas.height !== frame.displayHeight
      ) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
      frame.close();
    },
    error(e) {
      console.error("VideoDecoder error:", e);
    },
  });

  decoder.configure({
    // AV1 Main profile, level 4.1, Main tier, 8-bit — covers up to 1080p60.
    codec: "av01.0.09M.08",
    optimizeForLatency: true,
  });

  const pending = new Map<number, PendingFrame>();

  const tryDecode = (frameId: number, entry: PendingFrame) => {
    if (entry.received < entry.fragments.length) return;

    const totalBytes = entry.fragments.reduce((n, f) => n + f!.byteLength, 0);
    const data = new Uint8Array(totalBytes);
    let offset = 0;
    for (const frag of entry.fragments) {
      data.set(frag!, offset);
      offset += frag!.byteLength;
    }

    // Evict this frame and everything older (circular u16 order).
    for (const id of pending.keys()) {
      if (u16Leq(id, frameId)) pending.delete(id);
    }

    decoder.decode(
      new EncodedVideoChunk({
        type: entry.isKeyframe ? "key" : "delta",
        timestamp: frameId * 1000, // µs; ordering only, not wall-clock
        data,
      }),
    );
  };

  return [
    canvas,
    async () => {
      try {
        try {
          while (true) {
            const { value, done } = await wt.datagramReader!.read();
            if (done) break;
            if (!value) continue;

            const msg = parseMessage(value);
            if (!msg) continue;

            switch (msg.type) {
              case ServerMessageDiscriminant.VideoFrame: {
                let entry = pending.get(msg.frameId);
                if (!entry) {
                  entry = {
                    fragments: new Array(msg.numFrags).fill(null),
                    isKeyframe: false,
                    received: 0,
                  };
                  pending.set(msg.frameId, entry);
                }
                if (msg.isKeyframe) entry.isKeyframe = true;

                if (entry.fragments[msg.fragIdx] === null) {
                  entry.fragments[msg.fragIdx] = msg.payload;
                  entry.received++;
                  tryDecode(msg.frameId, entry);
                }
                break;
              }
            }
          }
        } catch (e) {
          console.log("Stream ended with error:", e);
        }

        await decoder
          .flush()
          .catch(() => {})
          .finally(() => decoder.close());

        ro.disconnect();
        document.removeEventListener("fullscreenchange", showResizePrompt);
        resizeOverlay?.remove();
        canvas.remove();

        const message = document.createElement("p");
        message.textContent = "Connection lost.";
        message.style.cssText =
          "position:fixed;inset:0;display:grid;place-items:center;margin:0;font-size:2rem;";
        document.body.appendChild(message);
      } catch (e) {
        console.error(e);
      }
    },
  ] as const;
};
