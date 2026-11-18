import { describe, expect, it } from 'vitest';
import {
  ChangePasswordInputSchema,
  AuthSignupInputSchema,
  PasswordStatusResponseSchema,
} from '../src/api/user.js';

describe('self password change contract', () => {
  it('admits 8..1024 UTF-16 units in signup and password changes without altering secrets', () => {
    for (const [password, accepted] of [
      ['short', false],
      ['1234567', false],
      ['12345678', true],
      [' a b c d ', true],
      ['x'.repeat(1024), true],
      ['x'.repeat(1025), false],
      ['😀'.repeat(512), true],
      ['😀'.repeat(513), false],
    ] as const) {
      const change = ChangePasswordInputSchema.safeParse({
        current_password: 'old',
        new_password: password,
      });
      const signup = AuthSignupInputSchema.safeParse({
        email: 'test@example.com',
        name: 'alice',
        password,
      });
      expect(change.success).toBe(accepted);
      expect(signup.success).toBe(accepted);
      if (change.success) expect(change.data.new_password).toBe(password);
      if (signup.success) expect(signup.data.password).toBe(password);
    }
  });
  it('rejects blank current passwords', () => {
    expect(
      ChangePasswordInputSchema.safeParse({ current_password: '', new_password: '12345678' })
        .success,
    ).toBe(false);
    expect(PasswordStatusResponseSchema.parse({ has_password: false })).toEqual({
      has_password: false,
    });
  });
});
