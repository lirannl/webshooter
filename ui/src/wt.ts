export const start = async () => {
    const wt = new WebTransport(location.href, { requireUnreliable: true });
    await wt.ready;
    let successes = 0;
    while (successes < 10) {
        const reader = wt.datagrams.readable.getReader();
        const data = await reader.read().then((v: ReadableStreamReadResult<string>) => { if (v.value) return v.value; throw new Error() }).catch(() => false as const);
        if (!data) break;
        const newDiv = document.createElement("div");
        newDiv.innerText = data;
        document.body.appendChild(newDiv);
        successes += 1;
    }
    wt.close();
}