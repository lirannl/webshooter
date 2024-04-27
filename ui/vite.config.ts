import { defineConfig } from "vite"
import { networkInterfaces } from "os";

const host = Object.values(networkInterfaces())?.flat().slice(1).filter(inter => inter?.family === "IPv4" && !inter.internal)?.map(inter => inter!.address)[0];

export default defineConfig({
    build: {
        outDir: "../dist",
    },
    server: {
        host: host,
        port: 5173,
        hmr: {
            port: 5173
        }
    }
});
