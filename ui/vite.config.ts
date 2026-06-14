import wasm from "vite-plugin-wasm";
import topLevelAwait from "vite-plugin-top-level-await";
import { defineConfig } from "vite";
import { spawn } from "child_process";
import chokidar from "chokidar";
import path from "path";

function wasmWatcher() {
    let isBuilding = false;
    let debounceTimer: NodeJS.Timeout | null = null;

    return {
        name: "wasm-watcher",
        apply: "serve" as const,
        configureServer(server) {
            const wasmSrc = path.resolve(__dirname, "wasm/src");
            const watcher = chokidar.watch(wasmSrc, {
                ignored: /(^|[/\\])\../,
                persistent: true,
                ignoreInitial: true,
            });

            const build = () => {
                if (isBuilding) return;
                isBuilding = true;
                console.log("[wasm-watcher] Building WASM...");
                const child = spawn("pnpm", ["build:wasm"], {
                    cwd: __dirname,
                    stdio: "inherit",
                    shell: true,
                });
                child.on("close", (code) => {
                    isBuilding = false;
                    if (code === 0) {
                        console.log("[wasm-watcher] WASM build complete, triggering HMR reload");
                        server.ws.send({ type: "full-reload" });
                    } else {
                        console.error(`[wasm-watcher] WASM build failed with code ${code}`);
                    }
                });
            };

            watcher.on("change", (file) => {
                console.log(`[wasm-watcher] Changed: ${file}`);
                if (debounceTimer) clearTimeout(debounceTimer);
                debounceTimer = setTimeout(build, 300);
            });
            watcher.on("add", (file) => {
                console.log(`[wasm-watcher] Added: ${file}`);
                if (debounceTimer) clearTimeout(debounceTimer);
                debounceTimer = setTimeout(build, 300);
            });
            watcher.on("unlink", (file) => {
                console.log(`[wasm-watcher] Removed: ${file}`);
                if (debounceTimer) clearTimeout(debounceTimer);
                debounceTimer = setTimeout(build, 300);
            });
        },
    };
}

export default defineConfig({
    build: {
        outDir: "../dist",
        emptyOutDir: true,
        target: "esnext",
    },
    server: {
        cors: true,
        hmr: {
            protocol: "http",
            clientPort: 5173,
        },
    },
    plugins: [wasm(), topLevelAwait(), wasmWatcher()],
});