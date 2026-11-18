export function hasTauriBackend(runtimeWindow: object): boolean {
  if (!('__TAURI_INTERNALS__' in runtimeWindow)) return false
  const internals = runtimeWindow.__TAURI_INTERNALS__
  return typeof internals === 'object'
    && internals !== null
    && 'invoke' in internals
    && typeof internals.invoke === 'function'
}

export const DESKTOP_APP_REQUIRED_MESSAGE =
  'Audio capture requires the Intervjuare macOS desktop app. The Vite browser preview has no Tauri backend, so it cannot discover or capture audio sources.'