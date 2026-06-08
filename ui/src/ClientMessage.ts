import { KeyboardInput, Modifiers } from "./input";
import { match } from "ts-pattern";
import { KeepAlive } from "./wt";

const ClientMessageConst = {
  KeepAlive: [0, {} as KeepAlive],
  Keyboard: [1, {} as KeyboardInput],
  Resize: [2, {} as ResizeDisplay],
} as const;

export const ClientMessageDiscriminant = Object.fromEntries(
  Object.entries(ClientMessageConst).map(([variant, [discriminant]]) => [
    variant,
    discriminant,
  ]),
) as {
  [K in keyof typeof ClientMessageConst]: (typeof ClientMessageConst)[K][0];
};

export type ClientMessageType = {
  [K in keyof typeof ClientMessageConst]: (typeof ClientMessageConst)[K][1];
};

export const toBytes = (
  message: ClientMessageType[keyof ClientMessageType],
) => {
  return match<typeof message, Uint8Array>(message)
    .with(
      { discriminant: ClientMessageDiscriminant.KeepAlive },
      (_) => new Uint8Array([ClientMessageDiscriminant.KeepAlive]),
    )
    .with(
      { discriminant: ClientMessageDiscriminant.Keyboard },
      (keyboardInput) => {
        const keycodeBytes = Uint8Array.from(keyboardInput.keycode, (c) =>
          c.charCodeAt(0),
        );
        const modifierByte = keyboardInput.modifiers.reduce(
          (flags, mod) => flags | Modifiers[mod],
          0,
        );
        const message = new Uint8Array(1 + 1 + keycodeBytes.length);
        message[0] = ClientMessageDiscriminant.Keyboard;
        message[1] = modifierByte;
        message.set(keycodeBytes, 2);
        return message;
      },
    )
    .with({ discriminant: ClientMessageDiscriminant.Resize }, (resize) => {
      const bytes = new Uint8Array(6);
      const view = new DataView(bytes.buffer);
      bytes[0] = resize.discriminant;
      bytes[1] = resize.index;
      view.setUint16(2, resize.width, false);
      view.setUint16(4, resize.height, false);
      return bytes;
    })
    .exhaustive();
};

export type ResizeDisplay = {
  discriminant: typeof ClientMessageDiscriminant.Resize;
  index: number;
  width: number;
  height: number;
};
