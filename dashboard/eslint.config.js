import js from "@eslint/js";
import reactHooks from "eslint-plugin-react-hooks";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["dist", "node_modules", "playwright-report", "test-results", "coverage"] },
  js.configs.recommended,
  ...tseslint.configs.recommendedTypeChecked,
  {
    languageOptions: {
      parserOptions: {
        projectService: { allowDefaultProject: ["eslint.config.js", "public/*.js"] },
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    files: ["**/*.{ts,tsx}"],
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      "@typescript-eslint/no-explicit-any": "error",
      "@typescript-eslint/consistent-type-imports": ["error", { fixStyle: "inline-type-imports" }],
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_", varsIgnorePattern: "^_" }],
      "@typescript-eslint/no-misused-promises": ["error", { checksVoidReturn: { attributes: false } }],
      "@typescript-eslint/restrict-template-expressions": ["error", { allowNumber: true, allowBoolean: true, allowNullish: true }],
      "@typescript-eslint/no-unnecessary-type-assertion": "error",
      "@typescript-eslint/no-floating-promises": ["error", { ignoreVoid: true }],
    },
  },
  {
    files: ["*.config.{js,ts}", "e2e/**/*.ts"],
    rules: { "@typescript-eslint/no-unsafe-assignment": "off" },
  },
  {
    // Plain browser script loaded before the bundle (see index.html); not part of the TS project.
    files: ["public/*.js"],
    languageOptions: {
      globals: { document: "readonly", localStorage: "readonly", matchMedia: "readonly" },
    },
    rules: { "@typescript-eslint/no-unused-vars": ["error", { caughtErrors: "none" }] },
  },
  {
    // TanStack Router signals redirects/not-found by throwing plain objects.
    files: ["src/router.tsx"],
    rules: { "@typescript-eslint/only-throw-error": "off" },
  },
);
