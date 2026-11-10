import { describe, expect, it } from 'vitest';
import {
  ChangePasswordInputSchema,
  HostStatusReadResponseSchema,
  HostStatusResponseSchema,
  PasswordStatusResponseSchema,
} from '../src/index.js';

// A response a newer server sends must still parse on an older client: desktop
// 0.4.8 failed the owner's computer page when the status body gained `view`.

const snapshot = {
  os: 'darwin',
  daemon_version: '0.3.3',
  daemon_pid: 42,
  started_at: 1,
  uptime_ms: 2,
};

describe('response schemas tolerate keys added by a newer server', () => {
  it('parses an owner status body carrying view and an unknown field', () => {
    const parsed = HostStatusResponseSchema.parse({
      host_id: 'h1',
      view: 'privileged',
      availability: 'available',
      snapshot: { ...snapshot, gpu: 'm4' },
      agents: [],
      added_later: true,
    });

    expect(parsed.availability).toBe('available');
  });

  it('parses a status body without view as the privileged tier', () => {
    const parsed = HostStatusReadResponseSchema.parse({
      host_id: 'h1',
      availability: 'unreachable',
      reason: 'timeout',
      agents: [],
    });

    expect(parsed.view).not.toBe('public');
    expect('reason' in parsed && parsed.reason).toBe('timeout');
  });

  it('resolves a public body with an unknown field to the public tier', () => {
    const parsed = HostStatusReadResponseSchema.parse({
      host_id: 'h1',
      view: 'public',
      availability: 'available',
      snapshot: { os: 'darwin' },
      agents: [],
      added_later: true,
    });

    expect(parsed.view).toBe('public');
  });

  it('parses a small response with an extra field', () => {
    expect(PasswordStatusResponseSchema.parse({ has_password: true, added_later: 1 })).toEqual({
      has_password: true,
    });
  });
});

describe('input schemas still reject unknown keys', () => {
  it('rejects an unknown key on a request body', () => {
    const body = { current_password: 'old-password-1', new_password: 'new-password-123' };
    expect(ChangePasswordInputSchema.safeParse(body).success).toBe(true);
    expect(
      ChangePasswordInputSchema.safeParse({
        current_password: 'old-password-1',
        new_password: 'new-password-123',
        unexpected: true,
      }).success,
    ).toBe(false);
  });
});
