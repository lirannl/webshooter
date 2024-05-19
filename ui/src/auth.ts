import { LoginParams } from "$rs/LoginParams";
import { bytesToBase64DataUrl, dataUrlToBytes } from "./base64";

export const subtle = crypto.subtle;
const ecdsaAlgoKeygen: EcKeyGenParams = {
    name: "ECDSA",
    namedCurve: "P-384",
};
const ecdsaAlgo: EcdsaParams = {
    name: "ECDSA",
    hash: {
        name: "SHA-384"
    }
}

export const ecdsaParams = <Usage extends "sign" | "verify">(usage: Usage | Usage[]) => [ecdsaAlgoKeygen, true, usage instanceof Array ? usage : [usage]] as const;

export const genKeyPair = async (): Promise<CryptoKeyPair> => {
    let signingKeyTxt: string | null = localStorage.getItem("privateKey");
    let verificationKeyTxt: string | null = localStorage.getItem("publicKey");

    if (!signingKeyTxt || !verificationKeyTxt) {
        const keyPair = await subtle.generateKey(...ecdsaParams(["sign", "verify"]));
        if (keyPair instanceof CryptoKey) throw new Error("Failed to generate ecdsa keys. Please try using another browser");

        signingKeyTxt = await bytesToBase64DataUrl(await subtle.exportKey("pkcs8", keyPair.privateKey));
        verificationKeyTxt = await bytesToBase64DataUrl(await subtle.exportKey("spki", keyPair.publicKey));

        localStorage.setItem("privateKey", signingKeyTxt);
        localStorage.setItem("publicKey", verificationKeyTxt);
    }

    const signingKey = await subtle.importKey("pkcs8", await dataUrlToBytes(signingKeyTxt), ...ecdsaParams("sign"));
    const verificationKey = await subtle.importKey("spki", await dataUrlToBytes(verificationKeyTxt), ...ecdsaParams("verify"));
    return {
        privateKey: signingKey,
        publicKey: verificationKey
    };
}

const toRawBase64 = async (buf: ArrayBuffer) => (await bytesToBase64DataUrl(buf)).split("base64,")[1]

export const getCookie = async (keypair: CryptoKeyPair) => {
    const pubKey = await subtle.exportKey("spki", keypair.publicKey);
    const id = await toRawBase64(pubKey);
    let idField = document.getElementById("signerId");
    if (!idField) {
        idField = document.createElement("div");
        idField.id = "signerId";
        document.body.appendChild(idField);
    }
    const idBytes = new Uint8Array(pubKey);
    idField.innerText = `Id summary: ${idBytes.slice(0, 4)}...${idBytes.slice(-4)}`;
    const challenge = await fetch("challenge", {
        headers: { id }
    });
    const challengeBlob = await ((await challenge.blob())).arrayBuffer();
    const signature = await subtle.sign(ecdsaAlgo, keypair.privateKey, challengeBlob);
    await subtle.verify(ecdsaAlgo, keypair.publicKey, signature, challengeBlob);

    fetch("/login", {
        method: "POST", headers: {
            credentials: "same-origin"
        }, body: JSON.stringify({
            Signature: {
                signature: await toRawBase64(signature),
                id,
            }
        } as LoginParams)
    })
}