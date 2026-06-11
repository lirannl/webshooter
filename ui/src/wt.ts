import {
  ClientMessageDiscriminant,
  ClientMessageType,
  toBytes,
} from "./ClientMessage";
import { handleKeyboard, handleTouch } from "./input";
import { render_video as prepareVideo } from "./video";

export type KeepAlive = {
  discriminant: typeof ClientMessageDiscriminant.KeepAlive;
};

const KeepAliveMessage = toBytes({
  discriminant: ClientMessageDiscriminant.KeepAlive,
});
export const start = async () => {
  const negotiation = await fetch("negotiate_wt");
  let token = negotiation.headers.get("token");
  const wt = new WebTransport(`${location.href}?token=${token}`, {
    requireUnreliable: true,
    serverCertificateHashes: [
      { algorithm: "sha-256", value: await negotiation.arrayBuffer() },
    ],
  });
  await wt.ready;
  // WASM entry point
  wt.datagramWriter = wt.datagrams.writable.getWriter();
  wt.datagramReader = wt.datagrams.readable.getReader();

  const keepAlive = setInterval(() => {
    wt.datagramWriter?.write(KeepAliveMessage);
  }, 50);

  const [canvas, startRender] = await prepareVideo(wt);

  handleKeyboard(wt, canvas);
  handleTouch(wt, canvas);

  await Promise.race([startRender(), wt.closed]);
  clearInterval(keepAlive);
};

export const send = async (
  wt: WebTransport,
  message: ClientMessageType[keyof ClientMessageType],
) => {
  const bytes = toBytes(message);
  if (bytes.byteLength >= wt.datagrams.maxDatagramSize) {
    const stream = await wt.createUnidirectionalStream();
    await stream.getWriter().write(bytes);
    await stream.close();
  } else {
    await wt.datagramWriter!.write(bytes);
  }
};
