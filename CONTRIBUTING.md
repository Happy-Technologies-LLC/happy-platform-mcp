# Contributing to Happy MCP Server

Thank you for your interest in contributing! This document provides guidelines for contributing to the Happy MCP Server project.

## Code of Conduct

Be respectful and constructive. We want this to be a welcoming community for everyone.

## How to Contribute

### Reporting Bugs

1. Check [existing issues](https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs/issues) to avoid duplicates.
2. Open a new issue with:
   - A clear title and description
   - Steps to reproduce
   - Expected vs. actual behavior
   - Your environment (Node.js version, OS, ServiceNow® version)

### Suggesting Features

Open an issue with the `enhancement` label. Describe:
- The problem you're trying to solve
- Your proposed solution
- Any alternatives you've considered

### Submitting Code

1. Fork the repository.
2. Create a feature branch from `main`:
   ```bash
   git checkout -b feature/your-feature-name
   ```
3. Make your changes, following the coding standards below.
4. Write or update tests as needed.
5. Run the test suite:
   ```bash
   npm test
   ```
6. Commit with clear, descriptive messages.
7. Push to your fork and open a pull request against `main`.

## Contributor License Agreement (CLA)

**All contributors must sign a Contributor License Agreement before their pull request can be merged.**

By submitting a pull request, you agree that:

1. You have the right to submit the contribution under the project's Apache 2.0 license.
2. You grant Happy Technologies LLC a perpetual, worldwide, non-exclusive, no-charge, royalty-free, irrevocable license to use, reproduce, modify, display, perform, sublicense, and distribute your contribution as part of this project.
3. You represent that your contribution is your original work, or you have the right to submit it.
4. You understand that your contribution will be licensed under the Apache License 2.0 and may be redistributed by Happy Technologies LLC and others.

**To sign the CLA:** Add the following statement to your first pull request description:

> I have read and agree to the Contributor License Agreement as described in CONTRIBUTING.md.

If you are contributing on behalf of your employer, please ensure you have authorization to do so.

## Coding Standards

- **Style:** Follow the existing code style. No linter is enforced, but be consistent.
- **Files:** Keep files under 500 lines where practical.
- **Tests:** Add tests for new features. Update tests for changed behavior.
- **Commits:** Write clear commit messages. One logical change per commit.
- **No secrets:** Never commit credentials, API keys, or instance URLs.
  CI runs `scripts/secret-scan.sh history` and `scripts/secret-scan.sh package` with gitleaks; run them locally (gitleaks 8.30.1 on `PATH`, full clone) before opening a PR. Fix false positives with a narrowly scoped `.gitleaks.toml` allowlist; never allowlist or baseline a live credential — rotate it first.

## Development Setup

```bash
# Clone your fork
git clone https://github.com/YOUR_USERNAME/mcp-servicenow-nodejs.git
cd mcp-servicenow-nodejs

# Install dependencies
npm install

# Copy config template
cp config/servicenow-instances.json.example config/servicenow-instances.json
# Edit with your test instance credentials

# Run in development mode
npm run dev

# Run tests
npm test
```

## Pull Request Guidelines

- Keep PRs focused on a single change.
- Include a clear description of what changed and why.
- Reference any related issues (e.g., "Fixes #42").
- Ensure all tests pass before requesting review.
- Be responsive to review feedback.

## Trademark Usage

When writing documentation or comments, please follow these guidelines:
- Use "ServiceNow®" (with ® symbol) on first reference per document.
- Do not use "ServiceNow" in feature names, tool names, or branding.
- Refer to this project as "Happy MCP Server," not "ServiceNow MCP Server."

## Questions?

Open a [discussion](https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs/discussions) or reach out via the issue tracker.

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
