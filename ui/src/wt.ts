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
  const cancelButton = document.createElement("button");
  cancelButton.textContent = "Cancel";
  document.body.appendChild(cancelButton);
  const closeFunc = () => {
    console.log("Bye!");
    wt.close();
  };
  cancelButton.addEventListener("click", closeFunc);

  while (true) {
    const { value, done } = await reader.read();
    if (done) {
      break;
    }
    // value is a Uint8Array.
    if (value) datagrams++;
    bytes += BigInt(value?.length ?? 0);
    try {
      const decoder = new TextDecoder();
      const text = decoder.decode(value);
      console.log(text);
    } catch {}
  }
  await wt.closed;

  clearInterval(stopWriting);
  cancelButton.removeEventListener("click", closeFunc);
  document.body.removeChild(cancelButton);
  wt.close();
};
