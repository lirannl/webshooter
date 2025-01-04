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
  // Needed to maintain the connection, apparently?
  const stopWriting = setInterval(() => {
    writer.write(new Uint8Array(1));
  }, 50);

  let datagrams = 0;
  let bytes = BigInt(0);

  setInterval(() => {
    console.log(`Received ${datagrams} datagrams. ${bytes} bytes in total`);
  }, 5000);

  while (true) {
    const { value, done } = await reader.read();
    if (done) {
      break;
    }
    // value is a Uint8Array.
    if (value) datagrams++;
    bytes += BigInt(value?.length ?? 0);
  }
  wt.closed.then(() => {
    clearInterval(stopWriting);
  });
  wt.close();
};
