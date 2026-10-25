import tailwindcss from '@tailwindcss/vite'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vitest/config'
import path from 'path'

// See https://vitejs.dev/config/
export default defineConfig({
	plugins: [
		vue(),
		tailwindcss(),
	],
	clearScreen: false,
	// Tauri v2 exposes its build variables as TAURI_ENV_*.
	envPrefix: ['VITE_', 'TAURI_ENV_'],
	server: {
		port: 1420,
		strictPort: true,
		fs: {
			allow: [
				// Optionally, include other directories
				path.resolve(import.meta.dirname)
			]
		}
	},
	build: {
		outDir: './dist',
		// WebView2 on Windows is Chromium; WebKitGTK on Linux and WKWebView on
		// macOS are WebKit. See https://v2.tauri.app/reference/webview-versions/
		target: process.env.TAURI_ENV_PLATFORM == 'windows' ? 'chrome105' : 'safari15',
		minify: !process.env.TAURI_ENV_DEBUG,
		sourcemap: !!process.env.TAURI_ENV_DEBUG,
		emptyOutDir: true,
	},
	// See https://vitest.dev/config/
	test: {
		include: ['tests/unit/**/*.{test,spec}.{js,mjs,cjs,ts,mts,cts,jsx,tsx}'],
	},
})
