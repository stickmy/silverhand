import parser from '@typescript-eslint/parser';
import hooks from 'eslint-plugin-react-hooks';
import stableHookDefault from './scripts/lint/stable-hook-default.mjs';

export default [
  {
    ignores: ['**/*.test.*', '**/__tests__/**', '**/node_modules/**', '**/.cache/**'],
  },
  {
    files: [
      'packages/app-ui/src/**/*.{ts,tsx}',
      'apps/desktop/src/renderer/**/*.{ts,tsx}',
      'apps/ios/src/**/*.{ts,tsx}',
    ],
    languageOptions: { parser },
    linterOptions: { reportUnusedDisableDirectives: 'off' },
    plugins: {
      'react-hooks': hooks,
      silverhand: { rules: { 'stable-hook-default': stableHookDefault } },
    },
    rules: {
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'error',
      'silverhand/stable-hook-default': 'error',
    },
  },
];
