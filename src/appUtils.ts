// Utility functions for meeting app detection
export interface CaptureSource {
  id: string;
  name: string;
  bundleId?: string;
}

/**
 * Determine if a capture source is a likely meeting app based on conservative matching.
 * Matches known meeting apps by name and/or bundle ID.
 */
export function isLikelyMeetingApp(source: CaptureSource): boolean {
  const { name, bundleId } = source;
  const lowerName = name.toLowerCase();

  // Known meeting apps: (name, optional bundleId)
  const knownApps: Array<{ name: string; bundleId?: string }> = [
    { name: 'Safari', bundleId: 'com.apple.Safari' },
    { name: 'Chrome', bundleId: 'com.google.Chrome' },
    { name: 'Chromium', bundleId: 'org.chromium.Chromium' },
    { name: 'Edge', bundleId: 'com.microsoft.Edge' },
    { name: 'Firefox', bundleId: 'org.mozilla.firefox' },
    { name: 'Arc', bundleId: 'com.company.Arc' }, // conservative; actual may differ
    { name: 'Teams', bundleId: 'com.microsoft.teams' },
  ];

  return knownApps.some(app => {
    const nameMatch = app.name.toLowerCase() === lowerName;
    if (app.bundleId && bundleId) {
      return nameMatch && app.bundleId.toLowerCase() === bundleId.toLowerCase();
    }
    return nameMatch;
  });
}