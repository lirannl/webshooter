import { base64ToBytes } from "./base64";

const authorise_webshooter_webtransport = async (wt: WebTransport) => {
    const token = await fetch("authorise_onetime");
    const tokenReceiver = (await wt.createUnidirectionalStream()).getWriter();
    await tokenReceiver.ready
    tokenReceiver.write(await token.arrayBuffer());
}

export const start = async () => {
    const negotiation = await fetch("negotiate_websocket");
    let token = negotiation.headers.get("token");
    const wt = new WebTransport(`${location.href}?token=${token}`, { requireUnreliable: true, serverCertificateHashes: [{ algorithm: "sha-256", value: await negotiation.arrayBuffer() }] });
    await wt.ready;
    const reader = wt.datagrams.readable.getReader();
    const writer = wt.datagrams.writable.getWriter();
    writer.write(new Uint8Array(1))

    while (true) {
        const { value, done } = await reader.read();
        if (done) {
            break;
        }
        // value is a Uint8Array.
        console.log(new TextDecoder().decode(value));
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