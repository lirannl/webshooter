import { defineConfig } from "vite"

export default defineConfig({
    build: {
        outDir: "../dist",
    },
    server: {
        cors: true,
        hmr: {
            protocol: "http",
            clientPort: 5173
        }
    }
});
