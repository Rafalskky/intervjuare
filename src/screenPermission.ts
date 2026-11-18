// Pure UI state logic for the explicit Screen & System Audio Recording action.
//
// Refresh Sources (`list_audio_sources`) is strictly passive: it never prompts.
// The explicit permission button below is the ONLY owner-initiated path that
// may trigger the system screen-capture prompt (via Tauri command
// `request_screen_capture_permission` -> Swift `request-screen-access`).
// Start still requires both a microphone and an application selection.

export const SCREEN_AUDIO_PERMISSION_DENIED = 'screen_audio_permission_denied'
export const SCREEN_AUDIO_PERMISSION_GRANTED = 'screen_audio_permission_granted'

export type DiscoveryErrorLike = { code: string; message?: string } | null | undefined

export function isScreenAudioPermissionDenied(code: string | null | undefined): boolean {
  return code === SCREEN_AUDIO_PERMISSION_DENIED
}

export function shouldShowScreenPermissionAction(
  applicationDiscoveryError: DiscoveryErrorLike,
): boolean {
  return isScreenAudioPermissionDenied(applicationDiscoveryError?.code)
}

export function isScreenPermissionActionEnabled(
  applicationDiscoveryError: DiscoveryErrorLike,
  running: boolean,
): boolean {
  return shouldShowScreenPermissionAction(applicationDiscoveryError) && !running
}

export function canStartCapture(microphoneId: string, applicationId: string): boolean {
  return Boolean(microphoneId) && Boolean(applicationId)
}

export function screenPermissionFollowupMessage(granted: boolean, backendMessage: string): string {
  // Truthful post-request guidance: a grant may not take effect until the app
  // is restarted; a denial keeps Start blocked until Settings + restart.
  if (granted) {
    return `${backendMessage} If applications still do not list, restart Intervjuare and use Refresh sources again.`
  }
  return `${backendMessage} Grant access in System Settings → Privacy & Security → Screen & System Audio Recording, then restart Intervjuare and use Refresh sources again.`
}
