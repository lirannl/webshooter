import { ClientMessageDiscriminant, toBytes } from "./ClientMessage";

export type KeyboardInput = {
  discriminant: typeof ClientMessageDiscriminant.Keyboard;
  keycode: string;
  modifiers: (keyof typeof Modifiers)[];
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
      canvas.requestFullscreen();
      // .then(() => navigator.keyboard?.lock());
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
    const input: KeyboardInput = {
      discriminant: ClientMessageDiscriminant.Keyboard,
      keycode: e.code,
      modifiers,
    };
    await wt.send(input);
  });
};
