export const bytesToBase64DataUrl = async (bytes: ArrayBuffer, type = "application/octet-stream") => {
    return await new Promise<string>((resolve, reject) => {
        const reader = Object.assign(new FileReader(), {
            onload: () => {
                if (typeof reader.result === "string") resolve(reader.result)
                else reject(reader.result);
            },
            onerror: () => reject(reader.error),
        });
        reader.readAsDataURL(new File([bytes], "", { type }));
    });
}

export const dataUrlToBytes = async (dataUrl: string) => {
    const res = await fetch(dataUrl);
    return await res.arrayBuffer();
}