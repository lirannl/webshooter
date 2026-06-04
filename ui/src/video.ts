// ── Server-to-client datagram protocol ───────────────────────────────────────
//
// First byte is always the message type (u8, up to 256 variants).
//
// VideoFrame (0x00):
//   [msg_type:1][frame_id:2][frag_idx:2][num_frags:2][flags:1][payload:N]
//   frame_id: u16 BE, wrapping circular counter
//   flags bit 0: keyframe (set on first fragment of a keyframe only)

const ServerMessageType = {
  VideoFrame: 0,
} as const;
type ServerMessageType =
  (typeof ServerMessageType)[keyof typeof ServerMessageType];

interface VideoFrameMsg {
  readonly type: typeof ServerMessageType.VideoFrame;
  frameId: number; // u16, wrapping
  fragIdx: number;
  numFrags: number;
  isKeyframe: boolean;
  payload: Uint8Array;
}

type ServerMessage = VideoFrameMsg;
// Future variants: | NextVariantMsg | ...

const HEADER = 8;

function parseMessage(data: Uint8Array): ServerMessage | null {
  if (data.byteLength < HEADER) return null;
  const view = new DataView(data.buffer, data.byteOffset);
  const msgType = view.getUint8(0);

  switch (msgType) {
    case ServerMessageType.VideoFrame:
      return {
        type: ServerMessageType.VideoFrame,
        frameId: view.getUint16(1),
        fragIdx: view.getUint16(3),
        numFrags: view.getUint16(5),
        isKeyframe: (view.getUint8(7) & 1) !== 0,
        payload: data.slice(HEADER),
      };
    default:
      return null;
  }
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

export const render_video = (
  reader: ReadableStreamDefaultReader<Uint8Array>,
  _wt: WebTransport,
): void => {
  const canvas = document.createElement("canvas");
  canvas.style.cssText =
    "position:fixed;inset:0;width:100%;height:100%;background:#000;cursor:pointer;";
  document.body.appendChild(canvas);
  const ctx = canvas.getContext("2d")!;

  canvas.addEventListener("click", () => {
    if (document.fullscreenElement === canvas) {
      document.exitFullscreen();
    } else {
      canvas.requestFullscreen();
    }
  });

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

  function tryDecode(frameId: number, entry: PendingFrame) {
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
  }

  (async () => {
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        if (!value) continue;

        const msg = parseMessage(value);
        if (!msg) continue;

        switch (msg.type) {
          case ServerMessageType.VideoFrame: {
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

    decoder
      .flush()
      .catch(() => {})
      .finally(() => decoder.close());

    canvas.remove();

    const message = document.createElement("p");
    message.textContent = "Connection lost.";
    message.style.cssText =
      "position:fixed;inset:0;display:grid;place-items:center;margin:0;font-size:2rem;";
    document.body.appendChild(message);
  })().catch(console.error);
};
