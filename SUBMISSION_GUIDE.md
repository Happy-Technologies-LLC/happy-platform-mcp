# MCP Server Directory Submissions

This guide tracks submissions to major MCP directories for the ServiceNow MCP Server.

## ✅ Already Published

### 1. Official MCP Registry
- **Status:** ✅ Published
- **URL:** https://registry.modelcontextprotocol.io/servers/io.github.nickzitzer/servicenow-nodejs
- **Version:** 2.1.1
- **Date:** 2025-10-17

### 2. npm Registry
- **Status:** ✅ Published
- **URL:** https://www.npmjs.com/package/servicenow-mcp-server
- **Version:** 2.1.1

### 3. Docker Hub
- **Status:** ✅ Published
- **URL:** https://hub.docker.com/r/nczitzer/mcp-servicenow-nodejs
- **Tags:** 2.1.1, latest

---

## 📋 Pending Submissions

### 1. Official Anthropic Server List (GitHub)
**Repository:** https://github.com/modelcontextprotocol/servers

**Submission Method:** Pull Request

**Steps:**
1. Fork the repository
2. Read CONTRIBUTING.md: https://github.com/modelcontextprotocol/servers/blob/main/CONTRIBUTING.md
3. Add server to appropriate category (alphabetically)
4. Submit PR with server details

**Server Entry Format:**
```markdown
### ServiceNow MCP Server
Multi-instance ServiceNow MCP server with 40+ tools, natural language search, and local script development.

- **Author:** nczitzer (Happy Technologies LLC)
- **Repository:** https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs
- **npm:** https://www.npmjs.com/package/servicenow-mcp-server
- **Docker:** https://hub.docker.com/r/nczitzer/mcp-servicenow-nodejs
- **MCP Registry:** https://registry.modelcontextprotocol.io/servers/io.github.nickzitzer/servicenow-nodejs
- **Features:**
  - 40+ MCP tools for ServiceNow operations
  - Multi-instance support
  - Natural language query interface
  - Local script development with Git integration
  - Update set management
  - Background script execution
  - Works on any ServiceNow table (94 with built-in schema)
```

**Status:** 🟡 Ready to submit

---

### 2. Cline's MCP Marketplace
**Repository:** https://github.com/cline/mcp-marketplace

**Submission Method:** GitHub Issue

**Steps:**
1. Go to: https://github.com/cline/mcp-marketplace/issues/new
2. Create issue with:
   - **GitHub Repo URL:** https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs
   - **Logo Image:** 400×400 PNG (use assets/logo.svg converted to PNG)
3. Wait for review (typically a couple days)

**Logo Preparation:**
```bash
# Convert SVG to PNG (400x400)
# Use ImageMagick or online converter
convert assets/logo.svg -resize 400x400 assets/logo-400x400.png
```

**Status:** 🟡 Ready to submit (need PNG logo)

---

### 3. Smithery
**Website:** https://smithery.ai/

**Submission Method:** Via website or GitHub

**Steps:**
1. Visit https://smithery.ai/
2. Look for "Submit Server" or "Add Server" option
3. Provide server details:
   - npm package: servicenow-mcp-server
   - GitHub: https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs
   - Description: Multi-instance ServiceNow MCP server with 40+ tools

**Alternative:** Check https://github.com/smithery-ai for submission guidelines

**Status:** 🔴 Need to find submission form

---

### 4. MCPHub.io
**Website:** https://mcphub.io/

**Submission Method:** Website form or automated discovery

**Details:**
- MCPHub indexes MCP servers from npm and GitHub
- May auto-discover from npm registry
- Check website for manual submission option

**Steps:**
1. Visit https://mcphub.io/
2. Search for "servicenow-mcp-server" to see if auto-indexed
3. If not found, look for submission form

**Status:** 🟡 Check if auto-indexed

---

### 5. mcp.run
**Website:** https://cloudmcp.run/

**Details:**
- Registry and control plane for MCP servers
- Integrates with official MCP registry
- May auto-discover from registry.modelcontextprotocol.io

**Steps:**
1. Check if already listed (since we're on official registry)
2. If not, contact via website

**Status:** 🟡 Check if auto-indexed

---

### 6. PulseMCP
**Website:** Community newsletter/hub

**Submission Method:** Newsletter submission or community post

**Steps:**
1. Find PulseMCP submission form or contact
2. Submit server announcement
3. Include:
   - Project name
   - Description
   - Key features
   - Links (GitHub, npm, Docker, MCP Registry)

**Status:** 🔴 Need to find submission process

---

### 7. mcpserverdirectory.com
**Website:** https://mcpserverdirectory.com

**Submission Method:** Website form

**Steps:**
1. Visit website
2. Find "Submit Server" or similar
3. Fill out form with server details

**Status:** 🔴 Need to access website

---

### 8. mcpnodes.com
**Website:** https://mcpnodes.com

**Submission Method:** Website form

**Steps:**
1. Visit website
2. Find submission form
3. Submit server details

**Status:** 🔴 Need to access website

---

### 9. OpenTools
**Website:** Open registry for MCP servers

**Submission Method:** TBD

**Status:** 🔴 Need to find website/submission process

---

## 📦 Server Details for Submissions

Use this standardized information for all submissions:

**Name:** ServiceNow MCP Server

**Short Description:** Multi-instance ServiceNow MCP server with 40+ tools and intelligent schema discovery

**Long Description:**
A revolutionary metadata-driven ServiceNow MCP server supporting multiple ServiceNow instances simultaneously with automatic schema discovery and optimized tool generation. Features 40+ MCP tools, natural language search, local script development with Git integration, and comprehensive ITSM operations.

**Author:** nczitzer / Happy Technologies LLC

**License:** MIT

**Category:** ITSM, Enterprise, Automation, ServiceNow

**Tags:** servicenow, itsm, mcp, automation, enterprise, multi-instance, workflow, cmdb, service-catalog

**Links:**
- GitHub: https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs
- npm: https://www.npmjs.com/package/servicenow-mcp-server
- Docker Hub: https://hub.docker.com/r/nczitzer/mcp-servicenow-nodejs
- MCP Registry: https://registry.modelcontextprotocol.io/servers/io.github.nickzitzer/servicenow-nodejs
- Documentation: https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs#readme

**Key Features:**
- 40+ MCP tools for ServiceNow operations
- Multi-instance support (connect to dev, test, prod simultaneously)
- Natural language query interface (query ServiceNow in plain English)
- Local script development with Git integration and watch mode
- Automated background script execution
- Update set management with batch operations
- Any ServiceNow table supported via generic tools (94 with built-in schema)
- Convenience tools for common ITSM operations
- Docker support for easy deployment
- Comprehensive documentation and examples

**Installation:**
```bash
# npm
npm install -g servicenow-mcp-server

# Docker
docker pull nczitzer/mcp-servicenow-nodejs:latest

# Via MCP Registry
# Use Claude Desktop MCP browser
```

**Quick Start:**
```bash
# Run with environment variables
export SERVICENOW_INSTANCE_URL=https://dev.service-now.com
export SERVICENOW_USERNAME=admin
export SERVICENOW_PASSWORD=password
servicenow-mcp-server
```

**Logo:**
- SVG: assets/logo.svg (teal/ServiceNow colors)
- PNG (400x400): assets/logo-400x400.png (needs conversion)

---

## 🎯 Action Items

### Immediate Actions:
1. ✅ Convert logo SVG to 400x400 PNG for Cline marketplace
2. 🔲 Submit PR to modelcontextprotocol/servers
3. 🔲 Submit issue to Cline's mcp-marketplace
4. 🔲 Check if auto-indexed on MCPHub.io and mcp.run
5. 🔲 Find Smithery submission process

### Research Needed:
- PulseMCP submission process
- mcpserverdirectory.com access
- mcpnodes.com submission form
- OpenTools platform details

---

## 📝 Submission Checklist Template

For each directory:
- [ ] Found submission process
- [ ] Prepared required materials (logo, description, etc.)
- [ ] Submitted application
- [ ] Received confirmation
- [ ] Server listed/approved
- [ ] Added listing URL to this document

---

## 📊 Submission Status Summary

| Directory | Status | Date Submitted | Date Approved | Listing URL |
|-----------|--------|----------------|---------------|-------------|
| Official MCP Registry | ✅ Live | 2025-10-17 | 2025-10-17 | https://registry.modelcontextprotocol.io/servers/io.github.nickzitzer/servicenow-nodejs |
| npm Registry | ✅ Live | 2025-10-17 | 2025-10-17 | https://www.npmjs.com/package/servicenow-mcp-server |
| Docker Hub | ✅ Live | 2025-10-17 | 2025-10-17 | https://hub.docker.com/r/nczitzer/mcp-servicenow-nodejs |
| Anthropic Servers (GitHub) | 🟡 Ready | - | - | - |
| Cline Marketplace | 🟡 Ready | - | - | - |
| MCPHub.io | 🟡 Check | - | - | - |
| mcp.run | 🟡 Check | - | - | - |
| Smithery | 🔴 Research | - | - | - |
| PulseMCP | 🔴 Research | - | - | - |
| mcpserverdirectory.com | 🔴 Research | - | - | - |
| mcpnodes.com | 🔴 Research | - | - | - |
| OpenTools | 🔴 Research | - | - | - |

**Legend:**
- ✅ Live - Published and available
- 🟡 Ready - Ready to submit, action needed
- 🟡 Check - May be auto-indexed, needs verification
- 🔴 Research - Need to find submission process

---

## 🔗 Useful Links

- MCP Specification: https://spec.modelcontextprotocol.io/
- MCP SDK: https://www.npmjs.com/package/@modelcontextprotocol/sdk
- Project Repository: https://github.com/Happy-Technologies-LLC/mcp-servicenow-nodejs
- Happy Technologies: https://happy-tech.biz

---

*Last Updated: 2025-10-17*
