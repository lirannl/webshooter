import { ClientMessageDiscriminant, toBytes } from "./ClientMessage";

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
