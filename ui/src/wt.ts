const authorise_webshooter_webtransport = async (wt: WebTransport) => {
    const token = await fetch("authorise_onetime");
    const tokenReceiver = (await wt.createUnidirectionalStream()).getWriter();
    await tokenReceiver.ready
    tokenReceiver.write(await token.arrayBuffer());
}

export const start = async () => {
    const hash = await fetch("webtransport_identity")
    const wt = new WebTransport(location.href, { requireUnreliable: true, serverCertificateHashes: [{ algorithm: "sha-256", value: await hash.arrayBuffer() }] });
    await wt.ready;
    await authorise_webshooter_webtransport(wt);
    const reader = wt.datagrams.readable.getReader();
    const writer = wt.datagrams.writable.getWriter();
    await writer.ready;
    {
        const input = document.createElement("input");
        document.body.appendChild(input);
        const listener = (event: Event) => {
            writer.write(new TextEncoder().encode((event as any).target.value))
        }
        input.addEventListener("input", listener)
    }

    while (true) {
        const { value, done } = await reader.read();
        if (done) {
            break;
        }
        // value is a Uint8Array.
        console.log(value);
    }

    // let successes = 0
    // while (successes < 10) {
    // const data = datagrams;
    // // .then((v: ReadableStreamReadResult<string>) => {
    // //     if (v.value) return v.value;
    // //     throw new Error()
    // // }).catch((err) => { console.error(err); false as const });
    // if (!data) break;
    // const newDiv = document.createElement("div");
    // newDiv.innerText = data.value;
    // document.body.appendChild(newDiv);
    // successes += 1;
    // }
    await new Promise(resolve => setTimeout(resolve, 60000));
    wt.close();
}