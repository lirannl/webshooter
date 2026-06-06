import { ClientMessageDiscriminant, toBytes } from "./ClientMessage";
import { handleKeyboard } from "./input";
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
  const reader: ReadableStreamDefaultReader<Uint8Array> =
    wt.datagrams.readable.getReader();
  const writer: WritableStreamDefaultWriter<Uint8Array> =
    wt.datagrams.writable.getWriter();

  const keepAlive = setInterval(() => {
    writer.write(KeepAliveMessage);
  }, 50);

  const [canvas, startRender] = await prepareVideo(reader, wt);

  handleKeyboard(writer, canvas);

  await Promise.race([startRender(), wt.closed]);
  clearInterval(keepAlive);
};
