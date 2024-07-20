import { defineConfig } from "vite"

export default defineConfig({
    build: {
        outDir: "../dist",
	target: "esnext",
    },
    server: {
        cors: true,
        hmr: {
            protocol: "http",
            clientPort: 5173
        }
    }
});
