import { createApp } from 'vue'
import App from './App.vue'
import { i18n } from './i18n'
import './styles/base.css'
import './styles/wizard.css'
import './styles/guide.css'

createApp(App).use(i18n).mount('#app')
