import { describe, it } from 'node:test';
import assert from 'node:assert';
import { isLikelyMeetingApp } from './appUtils.ts';

describe('isLikelyMeetingApp', () => {
  it('returns true for Safari by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Safari' }), true);
  });

  it('returns true for Safari by bundleId', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Safari', bundleId: 'com.apple.Safari' }), true);
  });

  it('returns true for Chrome by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Chrome' }), true);
  });

  it('returns true for Chrome by bundleId', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Chrome', bundleId: 'com.google.Chrome' }), true);
  });

  it('returns true for Chromium by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Chromium' }), true);
  });

  it('returns true for Edge by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Edge' }), true);
  });

  it('returns true for Firefox by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Firefox' }), true);
  });

  it('returns true for Arc by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Arc' }), true);
  });

  it('returns true for Teams by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Teams' }), true);
  });

  it('returns false for non-meeting app by name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Calculator' }), false);
  });

  it('is case-insensitive for name', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'safari' }), true);
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'SAFARI' }), true);
  });

  it('requires bundleId match when provided', () => {
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Safari', bundleId: 'com.apple.Safari' }), true);
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Safari', bundleId: 'com.google.Chrome' }), false);
  });

  it('returns false when name does not match even if bundleId matches (unlikely but testing logic)', () => {
    // Our function returns true only if name matches AND (if bundleId provided) bundleId matches.
    assert.strictEqual(isLikelyMeetingApp({ id: '1', name: 'Chrome', bundleId: 'com.apple.Safari' }), false);
  });
});