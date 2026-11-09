# real-chain fixture

Captured by `node scripts/gui/capture-real-chain.mjs --live` on 2026-11-09T09:05:59.266Z.

One turn driven by the real binary: scheduler, tool gateway, filesystem and `git diff`.

- Model: `deepseek-flash`
- Provider: OpenAI-compatible (`openai`)
- Events: 138
- Capture mode: live vendor endpoint

Redaction rewrites the throwaway workspace path to `<workspace>`. The capture also
replaces the API key, local user name and host name if they appear. This fixture
was checked against all four and only the workspace path was present.
