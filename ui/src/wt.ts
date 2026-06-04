import { render_video } from "./video";

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
  // Trigger the server to start the portal + pipeline.
  const stopWriting = setInterval(() => {
    writer.write(new Uint8Array(1));
  }, 50);

  render_video(reader, wt);

  await wt.closed;
  clearInterval(stopWriting);
};
