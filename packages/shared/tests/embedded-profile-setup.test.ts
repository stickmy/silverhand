import { describe, expect, it } from 'vitest';
import {
  CompleteEmbeddedProfileSetupInputSchema,
  EmbeddedProfileSetupResponseSchema,
} from '../src/api/user.js';
import { apiPaths } from '../src/api/paths.js';

describe('embedded profile setup contract', () => {
  it('uses the existing identity validators', () => {
    expect(
      CompleteEmbeddedProfileSetupInputSchema.parse({ name: 'Alice_1', display_name: ' Alice ' }),
    ).toEqual({ name: 'Alice_1', display_name: 'Alice' });
    for (const input of [
      { name: '123', display_name: 'Alice' },
      { name: 'Alice', display_name: '' },
    ])
      expect(CompleteEmbeddedProfileSetupInputSchema.safeParse(input).success).toBe(false);
  });

  it('requires explicit durable status and preserves the current viewer', () => {
    const user = {
      id: 'u1',
      name: 'Alice',
      display_name: 'Alice',
      avatar_url: null,
      created_at: 1,
      updated_at: 2,
    };
    expect(EmbeddedProfileSetupResponseSchema.parse({ required: false, user })).toEqual({
      required: false,
      user,
    });
    expect(EmbeddedProfileSetupResponseSchema.safeParse({ user }).success).toBe(false);
    expect(apiPaths.embeddedProfileSetup).toBe('/api/me/embedded-profile-setup');
  });
});
