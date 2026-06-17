/** @type {import('tailwindcss').Config} */
export default {
  content: [
    './templates/**/*.html',
    './src/**/*.rs',
  ],
  theme: {
    extend: {
      colors: {
        devbox: {
          bg: '#0b0f14',
          surface: '#111820',
          raised: '#1a2332',
          border: '#243044',
          subtle: '#2d3d54',
          accent: '#38bdf8',
          'accent-hover': '#7dd3fc',
          success: '#22c55e',
          'success-bg': '#0a2619',
          error: '#ef4444',
          'error-bg': '#2d1214',
          warning: '#eab308',
          'warning-bg': '#2d2208',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', '-apple-system', 'BlinkMacSystemFont', 'Segoe UI', 'Roboto', 'sans-serif'],
        mono: ['JetBrains Mono', 'SF Mono', 'Monaco', 'Consolas', 'monospace'],
      },
    },
  },
  plugins: [],
}
