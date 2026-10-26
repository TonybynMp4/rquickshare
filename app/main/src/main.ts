import { createApp } from 'vue'
import { createPinia } from 'pinia'

// Bundled rather than fetched from Google Fonts, so launching the app makes no
// third-party request and the font is there offline. 400 is body text, 500 is
// `font-medium`; nothing uses another weight.
import '@fontsource/montserrat/400.css'
import '@fontsource/montserrat/500.css'
import './vue_lib/assets/main.css'

import App from './App.vue'

// Only dial the standalone Vue devtools when it was actually started. `pnpm dev`
// launches it and sets this flag; `pnpm tauri dev` does not, and without the
// guard socket.io retries forever and floods the console with connection errors.
// Imported here rather than at the top so release builds don't bundle it.
if (import.meta.env.DEV && import.meta.env.VITE_VUE_DEVTOOLS === '1') {
	import('@vue/devtools').then(({ devtools }) => devtools.connect('http://localhost', 8098))
}

const pinia = createPinia();
const app = createApp(App)

app.use(pinia);

app.mount('#app')
