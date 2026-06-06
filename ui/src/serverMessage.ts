import { VideoFrameMsg } from "./video";

const ServerMessageConst = {
  VideoFrame: [0, {} as VideoFrameMsg],
} as const;

export const ServerMessageDiscriminant = Object.fromEntries(
  Object.entries(ServerMessageConst).map(([variant, [discriminant]]) => [
    variant,
    discriminant,
  ]),
) as {
  [K in keyof typeof ServerMessageConst]: (typeof ServerMessageConst)[K][0];
};

export type ServerMessageType = {
  [K in keyof typeof ServerMessageConst]: (typeof ServerMessageConst)[K][1];
};
export type ServerMessage =
  (typeof ServerMessageConst)[keyof typeof ServerMessageConst][1];

export function parseMessage(data: Uint8Array): ServerMessage | null {
  if (data.byteLength === 0) return null;
  const view = new DataView(data.buffer, data.byteOffset);
  const msgType = view.getUint8(0);

  switch (msgType) {
    case ServerMessageDiscriminant.VideoFrame:
      return {
        type: ServerMessageDiscriminant.VideoFrame,
        frameId: view.getUint16(1),
        fragIdx: view.getUint16(3),
        numFrags: view.getUint16(5),
        isKeyframe: (view.getUint8(7) & 1) !== 0,
        payload: data.slice(8),
      };
    default:
      return null;
  }
}
