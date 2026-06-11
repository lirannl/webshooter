import { ClientMessageDiscriminant } from "./ClientMessage";

export type KeyboardInput = {
  discriminant: typeof ClientMessageDiscriminant.Keyboard;
  keycode: string;
  modifiers: (keyof typeof Modifiers)[];
};

export type TouchscreenInput = {
  discriminant: typeof ClientMessageDiscriminant.Touchscreen;
  x: number;
  y: number;
  index: number;
};
export type TouchscreenRelease = {
  discriminant: typeof ClientMessageDiscriminant.TouchscreenRelease;
  index: number;
};

export const Modifiers = {
  shift: 1,
  ctrl: 2,
  alt: 4,
  meta: 8,
} as const;

export const handleKeyboard = (wt: WebTransport, canvas: HTMLCanvasElement) => {
  canvas.tabIndex = 0;
  canvas.focus();
  canvas.addEventListener(
    "click",
    () => {
      canvas
        .requestFullscreen()
        .then(() =>
          "keyboard" in navigator &&
          typeof navigator.keyboard === "object" &&
          !!navigator.keyboard &&
          "lock" in navigator.keyboard &&
          typeof navigator.keyboard.lock === "function"
            ? navigator.keyboard.lock()
            : null,
        );
    },
    { once: true },
  );
  canvas.addEventListener("keydown", async (e) => {
    e.preventDefault();
    const modifiers: (keyof typeof Modifiers)[] = [];
    if (e.shiftKey) modifiers.push("shift");
    if (e.ctrlKey) modifiers.push("ctrl");
    if (e.altKey) modifiers.push("alt");
    if (e.metaKey) modifiers.push("meta");
    await wt.send({
      discriminant: ClientMessageDiscriminant.Keyboard,
      keycode: e.code,
      modifiers,
    });
  });
};

const clamp = (value: number, lo: number, hi: number) =>
  value < lo ? lo : value > hi ? hi : value;

export const handleTouch = (wt: WebTransport, canvas: HTMLCanvasElement) => {
  // Stop the browser from interpreting touches as scroll / pinch-zoom
  // gestures. Without this the page pans instead of forwarding the events.
  canvas.style.touchAction = "none";

  // The portal addresses touch points by small, reusable slot numbers (u8),
  // but the browser hands out arbitrary (and possibly large) Touch.identifier
  // values. Map each active identifier to a compact slot, recycling on release.
  const slots = new Map<number, number>();
  const freeSlots: number[] = [];
  let nextSlot = 0;

  const acquireSlot = (id: number): number => {
    let slot = slots.get(id);
    if (slot === undefined) {
      slot = freeSlots.pop() ?? nextSlot++;
      slots.set(id, slot);
    }
    return slot;
  };

  // Coalesce moves to one send per frame: touchmove can fire far more often
  // than we want to put datagrams on the wire, so we keep only the latest
  // position per slot and flush them together on the next animation frame.
  const pending = new Map<number, { x: number; y: number }>();
  let rafHandle: number | null = null;

  const flush = () => {
    rafHandle = null;
    for (const [slot, point] of pending) {
      wt.send({
        discriminant: ClientMessageDiscriminant.Touchscreen,
        x: point.x,
        y: point.y,
        index: slot,
      });
    }
    pending.clear();
  };

  // Map a touch's screen position into the captured frame's pixel space, which
  // is the logical coordinate space the server forwards to the portal.
  const press = (e: TouchEvent) => {
    e.preventDefault();
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height || !canvas.width || !canvas.height) return;
    for (const touch of Array.from(e.changedTouches)) {
      const nx = (touch.clientX - rect.left) / rect.width;
      const ny = (touch.clientY - rect.top) / rect.height;
      pending.set(acquireSlot(touch.identifier), {
        x: clamp(Math.round(nx * canvas.width), 0, canvas.width - 1),
        y: clamp(Math.round(ny * canvas.height), 0, canvas.height - 1),
      });
    }
    if (rafHandle === null) rafHandle = requestAnimationFrame(flush);
  };
  canvas.addEventListener("touchstart", press, { passive: false });
  canvas.addEventListener("touchmove", press, { passive: false });

  const release = (e: TouchEvent) => {
    e.preventDefault();
    // Drain pending positions first so each slot's final down/move is
    // delivered before its release — matters for quick taps that start and
    // end within a single frame.
    if (rafHandle !== null) cancelAnimationFrame(rafHandle);
    flush();
    for (const touch of Array.from(e.changedTouches)) {
      const slot = slots.get(touch.identifier);
      if (slot === undefined) continue;
      slots.delete(touch.identifier);
      freeSlots.push(slot);
      wt.send({
        discriminant: ClientMessageDiscriminant.TouchscreenRelease,
        index: slot,
      });
    }
  };
  canvas.addEventListener("touchend", release, { passive: false });
  canvas.addEventListener("touchcancel", release, { passive: false });
};
