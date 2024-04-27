const rsaParams = {
    name: "RSA-OAEP",
    modulusLength: 4096,
    publicExponent: new Uint8Array([1, 0, 1]),
    hash: "SHA-256"
};

const hmac = {
    name: "HMAC",
    hash: { name: "SHA-512" }
};

const aesParams: AesKeyGenParams = { name: "AES", length: 512 };

const atob64 = (buffer: ArrayLike<number> | ArrayBufferLike) => {
    let binary = '';
    const bytes = new Uint8Array(buffer);
    const len = bytes.byteLength;
    for (let i = 0; i < len; i++) {
        binary += String.fromCharCode(bytes[i]);
    }
    return window.btoa(binary);
}

const b64toa = (b64: string): Uint8Array => {
    const unicode = window.atob(b64);

    const buf = [] as number[];
    for (let i = 0; i < unicode.length; i++)
        buf.push(unicode.charCodeAt(i));

    return new Uint8Array(buf);
}

const sessionSetup = async () => {
    const sessionKey = await window.crypto.subtle.generateKey(aesParams, true, ["encrypt", "decrypt"]);

    const sep = ";";
    let keyPair = localStorage.getItem("keypair")?.split(sep) as ([string, string] | undefined);
    if (!keyPair) {
        const pair = await window.crypto.subtle.generateKey(hmac, true, ["sign"]);
        const a: string = pair;
        keyPair = [atob64(await window.crypto.subtle.exportKey("raw", pair.publicKey)), atob64(await window.crypto.subtle.exportKey("raw", pair.privateKey))]
        localStorage.setItem("keypair", keyPair.join(sep));
    }

    const privKeyRaw = b64toa(keyPair[1]);
    const privKey = await window.crypto.subtle.importKey("pkcs8", privKeyRaw, rsaParams, true, ["sign"]);

    let res = await fetch("login", {
        headers: {
            pubkey: keyPair[0],
        }
    });
    if (!res.ok) throw new Error(await res.text());
    const challenge = await res.arrayBuffer();
    /*const challenge_signed = await crypto.subtle.sign(hmac, )

    res = await fetch("login/challenge", {
        headers: {
            pubkey: keyPair[0],
        }, method: "POST",
        body: challenge_signed
    });*/

    return sessionKey;
}
