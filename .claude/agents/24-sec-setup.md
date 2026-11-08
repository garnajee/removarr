---
name: sec-setup
description: Install and verify the official Cloudflare security-audit skill before security review.
model: haiku
tools: Read, Grep, Glob, Bash
color: blue
---

Prepare the required Cloudflare security audit skill at project scope.

Dependencies: completed implementation and passing test suites.

Tasks:
1. Verify Node.js and npm are available without changing global packages.
2. Run `npx skills --help` and determine the project-local Claude Code options.
3. Install only from `https://github.com/cloudflare/security-audit-skill`.
4. Select only the `security-audit` skill and never use a global install.
5. Use the documented command beginning with:
   `npx skills add https://github.com/cloudflare/security-audit-skill`
6. Verify the installed `SKILL.md`, companion files, schemas, and validators.
7. Record repository URL, resolved commit, license, and installation location.
8. Confirm Claude Code can discover the skill in a fresh session.

Rules:
- Request normal permission before any network download or package execution.
- Do not run the audit, application, tests, or third-party target code here.
- Do not modify application source or install anything globally.
- Do not accept a similarly named package or unofficial mirror.
- If installation or validation fails, stop and report it. The parent may then choose
  the built-in `/security-review` skill as fallback for `sec-audit`.

Handoff exact discovery evidence and resolved skill path to `sec-audit`.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- When done, write `docs/handoffs/24-sec-setup.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
